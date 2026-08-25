//! Default agent behavior that wires an LLM provider + token tracking + checkpointing.
//! This is the standard "call the LLM" behavior most agents will use.

use std::sync::Arc;

use axocoatl_core::{
    AgentConfig, AgentInput, AgentOutput, ChatMessage, ConversationMode, MessageRole,
    OverflowPolicy, TokenUsageStats,
};
use axocoatl_llm::{ChatRequest, LlmProvider, ToolCall};
use axocoatl_memory::{AgentCheckpoint, CheckpointStore, SessionMemory, StoredMessage};
use axocoatl_token::{BudgetError, TokenCounter, TokenTracker};
use axocoatl_tools::{ConcurrentToolDispatcher, HookRegistry, ProviderToolNameMap, ToolExecutor};

use crate::behavior::{AgentBehavior, ExecutionUsageState};
use crate::error::AgentError;
use crate::run_control::{AgentRunControl, AgentRunOutcome};

const PROJECT_INSTRUCTION_FILE_MAX_BYTES: usize = 64 * 1024;
const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 256 * 1024;
const COMPACTION_ARCHIVE_MAX_BYTES: usize = 1024 * 1024;
const COMPACTION_ARCHIVE_MESSAGE_MAX_BYTES: usize = 64 * 1024;
const COMPACTION_ARCHIVE_FIELD_MAX_BYTES: usize = 16 * 1024;
const COMPACTION_ARCHIVE_ENVELOPE_RESERVE_BYTES: usize = 4 * 1024;
const MAX_PROVIDER_TOOL_CALLS: usize = 128;
const MAX_TEXT_JSON_CANDIDATES: usize = MAX_PROVIDER_TOOL_CALLS * 2;

/// Plain-text JSON tool recovery is a compatibility path for Ollama models
/// that sometimes omit their structured function-call channel. Hosted/native
/// protocols require provider-issued replay state (Mistral ids, Gemini thought
/// signatures, Anthropic native blocks) that Axocoatl cannot safely invent.
fn supports_text_tool_recovery(provider: &str) -> bool {
    provider == "ollama"
}

/// A deterministic, portable non-empty correlation id. Ollama accepts this
/// shape, and its exact nine ASCII-alphanumeric bytes also remain compatible
/// with the narrowest shipped id validator (Mistral), preventing an accidental
/// invalid history shape if the call is inspected or migrated.
fn text_tool_recovery_id(index: usize) -> Result<String, AgentError> {
    if index >= MAX_PROVIDER_TOOL_CALLS {
        return Err(AgentError::Provider(format!(
            "provider text recovery exceeded the portable {MAX_PROVIDER_TOOL_CALLS}-call limit"
        )));
    }
    Ok(format!("Axo{index:06X}"))
}

/// Securely compose repository instructions for any actor behavior that runs in
/// a directory Session. Traversal stays relative to opened directory handles so
/// linked paths cannot expose host files to the model.
pub(crate) fn load_project_instructions(working_dir: &std::path::Path) -> Option<String> {
    let mut chunks: Vec<(std::path::PathBuf, String)> = Vec::new();
    let mut instruction_bytes = 0_usize;
    let read_candidate = |directory: &axocoatl_core::SecureDir,
                          chunks: &mut Vec<(std::path::PathBuf, String)>,
                          instruction_bytes: &mut usize| {
        let remaining = PROJECT_INSTRUCTIONS_MAX_BYTES.saturating_sub(*instruction_bytes);
        if remaining == 0 {
            return;
        }
        let limit = PROJECT_INSTRUCTION_FILE_MAX_BYTES.min(remaining);
        let Ok(bytes) = directory.read_limited("AXOCOATL.md", limit) else {
            return;
        };
        *instruction_bytes = instruction_bytes.saturating_add(bytes.len());
        let Ok(text) = String::from_utf8(bytes) else {
            return;
        };
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            chunks.push((directory.path().join("AXOCOATL.md"), trimmed.to_string()));
        }
    };

    #[cfg(unix)]
    {
        let absolute = working_dir.is_absolute();
        let mut directory = if absolute {
            axocoatl_core::SecureDir::open(std::path::Path::new("/"))
        } else {
            std::env::current_dir().and_then(axocoatl_core::SecureDir::open)
        };
        let components = working_dir.components().skip(usize::from(absolute));
        let Ok(ref current) = directory else {
            return None;
        };
        read_candidate(current, &mut chunks, &mut instruction_bytes);
        for component in components {
            let std::path::Component::Normal(name) = component else {
                break;
            };
            directory = directory.and_then(|current| current.existing_child(name));
            let Ok(ref current) = directory else {
                break;
            };
            read_candidate(current, &mut chunks, &mut instruction_bytes);
        }
    }

    #[cfg(not(unix))]
    {
        let mut ancestors: Vec<&std::path::Path> = working_dir.ancestors().collect();
        ancestors.reverse();
        for ancestor in ancestors {
            if let Ok(directory) = axocoatl_core::SecureDir::open(ancestor) {
                read_candidate(&directory, &mut chunks, &mut instruction_bytes);
            }
        }
    }

    if chunks.is_empty() {
        return None;
    }
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
    Some(composed)
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn bounded_archive_message(message: &ChatMessage) -> serde_json::Value {
    let full = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    let full_bytes = serde_json::to_vec(&full).map_or(0, |bytes| bytes.len());
    if full_bytes <= COMPACTION_ARCHIVE_MESSAGE_MAX_BYTES {
        return full;
    }

    let mut bounded = message.clone();
    bounded.content = match bounded.content {
        axocoatl_core::MessageContent::Text(content) => {
            let prefix = utf8_prefix(&content, COMPACTION_ARCHIVE_FIELD_MAX_BYTES);
            axocoatl_core::MessageContent::Text(format!(
                "{prefix}\n[archive truncated: original_content_bytes={}]",
                content.len()
            ))
        }
        axocoatl_core::MessageContent::Parts(parts) => axocoatl_core::MessageContent::Parts(
            parts
                .into_iter()
                .map(|part| match part {
                    axocoatl_core::ContentPart::Text(content) => {
                        let prefix = utf8_prefix(&content, COMPACTION_ARCHIVE_FIELD_MAX_BYTES);
                        axocoatl_core::ContentPart::Text(format!(
                            "{prefix}\n[archive truncated: original_content_bytes={}]",
                            content.len()
                        ))
                    }
                    axocoatl_core::ContentPart::Image { url, detail } => {
                        axocoatl_core::ContentPart::Image {
                            url: format!(
                                "[archive omitted image transport: original_url_bytes={}]",
                                url.len()
                            ),
                            detail,
                        }
                    }
                })
                .collect(),
        ),
    };
    for call in &mut bounded.tool_calls {
        let arguments = serde_json::to_string(&call.arguments).unwrap_or_default();
        if arguments.len() > COMPACTION_ARCHIVE_FIELD_MAX_BYTES {
            call.arguments = serde_json::json!({
                "archive_truncated": true,
                "original_bytes": arguments.len(),
                "preview": utf8_prefix(&arguments, COMPACTION_ARCHIVE_FIELD_MAX_BYTES),
            });
        }
        for value in call.provider_metadata.values_mut() {
            if value.len() > COMPACTION_ARCHIVE_FIELD_MAX_BYTES {
                *value = format!(
                    "{}[archive truncated: original_bytes={}]",
                    utf8_prefix(value, COMPACTION_ARCHIVE_FIELD_MAX_BYTES),
                    value.len()
                );
            }
        }
    }

    let mut value = serde_json::to_value(bounded).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "archive_truncated".to_string(),
            serde_json::Value::Bool(true),
        );
        object.insert(
            "archive_original_bytes".to_string(),
            serde_json::json!(full_bytes),
        );
    }
    value
}

fn structured_compaction_archive(messages: &[ChatMessage]) -> (Vec<serde_json::Value>, usize) {
    let mut retained_reversed = Vec::new();
    let mut retained_bytes = 0usize;
    let mut omitted_messages = 0usize;
    let message_budget =
        COMPACTION_ARCHIVE_MAX_BYTES.saturating_sub(COMPACTION_ARCHIVE_ENVELOPE_RESERVE_BYTES);
    for message in messages.iter().rev() {
        let bounded = bounded_archive_message(message);
        let bytes = serde_json::to_vec(&bounded).map_or(0, |serialized| serialized.len());
        // Include the comma needed between JSON array entries. The reserved
        // envelope covers the fixed archive fields and object/array framing.
        let delimiter = usize::from(!retained_reversed.is_empty());
        if retained_bytes
            .saturating_add(bytes)
            .saturating_add(delimiter)
            > message_budget
        {
            // Keep one contiguous newest suffix. Retaining an older small
            // message after omitting a newer large one would make the archive
            // look like a complete provider transaction when it is not.
            omitted_messages = messages.len().saturating_sub(retained_reversed.len());
            break;
        }
        retained_bytes = retained_bytes
            .saturating_add(bytes)
            .saturating_add(delimiter);
        retained_reversed.push(bounded);
    }
    retained_reversed.reverse();
    (retained_reversed, omitted_messages)
}

struct StreamChatResult {
    response: axocoatl_llm::ChatResponse,
    cancelled: bool,
    /// True when the stream produced a terminal Done or an exact Usage event.
    /// A cancelled nonterminal stream may carry a useful local numeric
    /// estimate, but its remote total remains incomplete.
    usage_complete: bool,
    provider_tool_names: ProviderToolNameMap,
    provider_route: axocoatl_core::ProviderMetadata,
}

fn merge_provider_metadata(
    target: &mut axocoatl_core::ProviderMetadata,
    source: axocoatl_core::ProviderMetadata,
    conflict: &'static str,
) -> Result<(), AgentError> {
    for (key, value) in source {
        if target.get(&key).is_some_and(|existing| existing != &value) {
            return Err(AgentError::Provider(conflict.to_string()));
        }
        target.insert(key, value);
    }
    Ok(())
}

/// Default behavior: builds ChatRequest from input, calls LLM provider, tracks tokens,
/// maintains session memory, executes tool calls, and optionally checkpoints.
pub struct DefaultAgentBehavior {
    provider: Arc<dyn LlmProvider>,
    /// Configured enforcement policy. A fresh tracker is created for each
    /// Execute activation; actor-lifetime reporting is kept separately in
    /// `cumulative_token_usage` and never consumes a later turn's allowance.
    token_budget: Option<axocoatl_core::TokenBudget>,
    tracker: Option<TokenTracker>,
    /// Actor-lifetime provider usage, independent of whether an enforcement
    /// budget is configured. Reasoning remains a separate dimension here;
    /// only the optional tracker folds it into charged output headroom.
    cumulative_token_usage: ExecutionUsageState,
    /// Usage for the current/most recent Execute, including whether every
    /// dispatched provider call yielded terminal exact/estimated usage.
    execution_usage: ExecutionUsageState,
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
    /// Canonical executor-tool allowlist. `None` inherits the full executor;
    /// `Some`, including an empty set, is an exact allowlist. Agent-scoped
    /// recall/core-memory tools are intrinsic and are intentionally separate.
    executor_tool_allowlist: Option<std::collections::HashSet<String>>,
    hook_registry: Option<Arc<HookRegistry>>,
    /// Optional append-only daily-log cache. When configured, compaction writes
    /// a bounded structured projection here before summarizing. Canonical
    /// Session/Chat history remains the durable owner of the exact transcript.
    daily_log: Option<Arc<axocoatl_memory::DailyLogMemory>>,
    /// Core memory (Tier 3) — agent-editable, curated blocks rendered into the
    /// system prompt and maintained via the core-memory tools. It is the curated,
    /// intentionally lossy top of the hierarchy; canonical Session/Chat history
    /// owns the exact transcript.
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
    /// from the personal `core_memory` and `semantic_memory` which are
    /// per-user.
    project_instructions: Option<String>,
    /// Set by the actor before a streaming execution — receives output chunks
    /// as the LLM generates them.
    stream_sink: Option<crate::behavior::StreamSink>,
    /// Caller-owned control for the active execution, when the daemon needs a
    /// reconnect-safe Stop action. Agent actors serialize their executions.
    active_run_control: Option<AgentRunControl>,
    /// Set only after this behavior observes cancellation at a safe boundary.
    active_run_cancelled: bool,
    /// Per-agent sampling controls (temperature, top_p, max_tokens, response
    /// format), applied to every ChatRequest this agent builds.
    sampling: axocoatl_core::SamplingConfig,
}

impl DefaultAgentBehavior {
    pub fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            provider,
            token_budget: None,
            tracker: None,
            cumulative_token_usage: ExecutionUsageState::default(),
            execution_usage: ExecutionUsageState::default(),
            counter,
            system_prompt: None,
            configured_model: None,
            session: SessionMemory::new(),
            checkpoint_store: None,
            checkpoint_version: 0,
            agent_id: String::new(),
            tool_executor: None,
            executor_tool_allowlist: None,
            hook_registry: None,
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
            active_run_control: None,
            active_run_cancelled: false,
            sampling: axocoatl_core::SamplingConfig::default(),
        }
    }

    /// Set the per-agent sampling controls applied to every LLM call.
    pub fn with_sampling(mut self, sampling: axocoatl_core::SamplingConfig) -> Self {
        self.sampling = sampling;
        self
    }

    /// Consume the provider's token stream — forwarding each text/reasoning
    /// delta to the stream sink (if attached) — and assemble the equivalent
    /// `ChatResponse`. Used in place of the blocking `provider.chat()` so
    /// every agent call is live by default.
    async fn stream_chat(
        &self,
        request: ChatRequest,
        provider_tool_names: ProviderToolNameMap,
    ) -> Result<StreamChatResult, AgentError> {
        use axocoatl_llm::{ChatResponse, FinishReason, StreamEvent, ToolCall};
        use tokio_stream::StreamExt;

        // `request` is already provider-encoded and was measured in exactly
        // this wire form by the context and spend preflights. Keep the same map
        // for response decoding; remapping here would make the checked request
        // differ from the request that crosses the provider boundary.
        let mut provider_route = axocoatl_core::ProviderMetadata::new();
        let control = self.active_run_control.clone();
        let provider_id = self.provider.provider_id().to_string();
        let empty_response = || ChatResponse {
            content: String::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::default(),
            model: String::new(),
            provider: provider_id.clone(),
        };

        if control.as_ref().is_some_and(AgentRunControl::is_cancelled) {
            return Ok(StreamChatResult {
                response: empty_response(),
                cancelled: true,
                usage_complete: true,
                provider_tool_names,
                provider_route,
            });
        }

        // Dropping the provider future/stream is the strongest generic abort
        // available through LlmProvider today. Provider transports then close
        // their HTTP body/connection without waiting for the remaining tokens.
        let stream_result = if let Some(control) = &control {
            tokio::select! {
                biased;
                _ = control.cancelled() => {
                    return Ok(StreamChatResult {
                        response: empty_response(),
                        cancelled: true,
                        usage_complete: true,
                        provider_tool_names,
                        provider_route,
                    });
                }
                result = async {
                    self.begin_provider_call();
                    self.provider.chat_stream(request).await
                } => result,
            }
        } else {
            self.begin_provider_call();
            self.provider.chat_stream(request).await
        };
        let mut stream = stream_result.map_err(|e| AgentError::Provider(e.to_string()))?;

        let mut content = String::new();
        let mut usage = TokenUsageStats::default();
        let mut saw_usage = false;
        macro_rules! fail_stream {
            ($error:expr) => {
                return Err(self.account_reported_stream_usage_on_error(&usage, saw_usage, $error))
            };
        }
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
            provider_metadata: axocoatl_core::ProviderMetadata,
        }
        let mut tool_accum: Vec<ToolAccum> = Vec::new();

        let mut cancelled = false;
        let mut saw_done = false;
        loop {
            let next = if let Some(control) = &control {
                tokio::select! {
                    biased;
                    _ = control.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    event = stream.next() => event,
                }
            } else {
                stream.next().await
            };
            let Some(ev) = next else { break };
            let event = match ev {
                Ok(event) => event,
                Err(error) => {
                    return Err(self.account_reported_stream_usage_on_error(
                        &usage,
                        saw_usage,
                        AgentError::Provider(error.to_string()),
                    ));
                }
            };
            match event {
                StreamEvent::ProviderRoute { metadata } => {
                    if !provider_route.is_empty() && provider_route != metadata {
                        fail_stream!(AgentError::Provider(
                            "provider stream reported conflicting selected routes".to_string(),
                        ));
                    }
                    provider_route = metadata;
                }
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
                    let name = name.filter(|name| !name.is_empty());
                    if index.is_none() && id.is_empty() && name.is_none() {
                        fail_stream!(AgentError::Provider(
                            "provider streamed a tool-call fragment without an index, id, or name"
                                .to_string(),
                        ));
                    }
                    let pos = tool_accum.iter().position(|t| match (t.index, index) {
                        (Some(a), Some(b)) => a == b,
                        _ => !id.is_empty() && t.id == id,
                    });
                    match pos {
                        Some(i) => {
                            let t = &mut tool_accum[i];
                            if !t.id.is_empty() && !id.is_empty() && t.id != id {
                                fail_stream!(AgentError::Provider(
                                    "provider changed a streamed tool-call id for one index"
                                        .to_string(),
                                ));
                            }
                            if t.id.is_empty() && !id.is_empty() {
                                t.id = id;
                            }
                            if t.index.is_none() {
                                t.index = index;
                            }
                            if let Some(n) = name {
                                if !t.name.is_empty() && t.name != n {
                                    fail_stream!(AgentError::Provider(
                                        "provider changed a streamed tool-call name".to_string(),
                                    ));
                                }
                                t.name = n;
                            }
                            t.args.push_str(&args_delta);
                        }
                        None => {
                            if tool_accum.len() >= MAX_PROVIDER_TOOL_CALLS {
                                fail_stream!(AgentError::Provider(format!(
                                    "provider streamed more than {MAX_PROVIDER_TOOL_CALLS} distinct tool calls"
                                )));
                            }
                            tool_accum.push(ToolAccum {
                                index,
                                id,
                                name: name.unwrap_or_default(),
                                args: args_delta,
                                provider_metadata: Default::default(),
                            });
                        }
                    }
                }
                StreamEvent::ToolCallMetadata {
                    index,
                    id,
                    metadata,
                } => {
                    let position = tool_accum
                        .iter()
                        .position(|tool| match (tool.index, index) {
                            (Some(left), Some(right)) => left == right,
                            _ => !id.is_empty() && tool.id == id,
                        });
                    let Some(position) = position else {
                        fail_stream!(AgentError::Provider(
                            "provider streamed tool-call metadata before its matching call"
                                .to_string(),
                        ));
                    };
                    let tool = &mut tool_accum[position];
                    if !tool.id.is_empty() && !id.is_empty() && tool.id != id {
                        fail_stream!(AgentError::Provider(
                            "provider attached tool-call metadata to a conflicting id".to_string(),
                        ));
                    }
                    if tool.id.is_empty() && !id.is_empty() {
                        tool.id = id;
                    }
                    if tool.index.is_none() {
                        tool.index = index;
                    }
                    merge_provider_metadata(
                        &mut tool.provider_metadata,
                        metadata,
                        "provider changed streamed metadata for one tool call",
                    )
                    .map_err(|error| {
                        self.account_reported_stream_usage_on_error(&usage, saw_usage, error)
                    })?;
                }
                StreamEvent::Usage(u) => {
                    usage = u;
                    saw_usage = true;
                }
                StreamEvent::Done { finish_reason: fr } => {
                    finish_reason = fr;
                    saw_done = true;
                    // This is the provider completion boundary. A Stop request
                    // received after it must not relabel a completed response.
                    break;
                }
            }
        }

        if !cancelled && !saw_done {
            fail_stream!(AgentError::Provider(
                "provider stream ended before its completion event".to_string(),
            ));
        }

        let tool_calls = if cancelled {
            // Cancellation drops partial native call state. Parsing an
            // intentionally interrupted argument buffer would relabel a clean
            // Stop as a provider protocol failure.
            Vec::new()
        } else {
            tool_accum
                .into_iter()
                .map(|t| {
                    let Some(name) = provider_tool_names.decode_advertised_name_owned(t.name)
                    else {
                        return Err(AgentError::Provider(
                            "provider returned an empty or undeclared tool-call name".to_string(),
                        ));
                    };
                    let arguments: serde_json::Value =
                        serde_json::from_str(&t.args).map_err(|_| {
                            AgentError::Provider(
                                "provider returned malformed or incomplete tool-call arguments"
                                    .to_string(),
                            )
                        })?;
                    if !arguments.is_object() {
                        return Err(AgentError::Provider(
                            "provider returned non-object tool-call arguments".to_string(),
                        ));
                    }
                    let mut provider_metadata = provider_route.clone();
                    merge_provider_metadata(
                        &mut provider_metadata,
                        t.provider_metadata,
                        "provider tool-call metadata conflicts with its selected route",
                    )?;
                    Ok(ToolCall {
                        id: t.id,
                        name,
                        arguments,
                        provider_metadata,
                    })
                })
                .collect::<Result<Vec<_>, AgentError>>()
                .map_err(|error| {
                    self.account_reported_stream_usage_on_error(&usage, saw_usage, error)
                })?
        };
        if !cancelled && matches!(finish_reason, FinishReason::ToolUse) && tool_calls.is_empty() {
            fail_stream!(AgentError::Provider(
                "provider completed with tool-use finish reason but no tool calls".to_string(),
            ));
        }
        if !cancelled && !matches!(&finish_reason, FinishReason::ToolUse) && !tool_calls.is_empty()
        {
            fail_stream!(AgentError::Provider(
                "provider returned tool calls under a non-tool finish reason".to_string(),
            ));
        }

        let selected_provider = provider_route
            .get(axocoatl_llm::TOOL_METADATA_ROUTE_PROVIDER)
            .cloned()
            .unwrap_or(provider_id);
        let selected_model = provider_route
            .get(axocoatl_llm::TOOL_METADATA_ROUTE_MODEL)
            .cloned()
            .unwrap_or_default();
        Ok(StreamChatResult {
            response: ChatResponse {
                content,
                // A partial tool-call delta is not a safe action to execute.
                tool_calls,
                finish_reason,
                usage,
                model: selected_model,
                provider: selected_provider,
            },
            cancelled,
            usage_complete: saw_done || saw_usage,
            provider_tool_names,
            provider_route,
        })
    }

    /// Encode canonical executor/history names exactly once before any
    /// provider-sensitive context or spend measurement. MCP names can be short
    /// but invalid/reserved (and expand to a 64-byte alias), or very long (and
    /// shrink); only the encoded request is the true provider wire cost.
    fn encode_provider_request(
        request: ChatRequest,
    ) -> Result<(ChatRequest, ProviderToolNameMap), AgentError> {
        let provider_tool_names = ProviderToolNameMap::for_request(&request)
            .map_err(|error| AgentError::Internal(error.to_string()))?;
        let request = provider_tool_names.encode_request(request);
        Ok((request, provider_tool_names))
    }

    fn cancellation_requested(&self) -> bool {
        self.active_run_control
            .as_ref()
            .is_some_and(AgentRunControl::is_cancelled)
    }

    fn observe_cancellation(&mut self) -> bool {
        if self.cancellation_requested() {
            self.active_run_cancelled = true;
            true
        } else {
            false
        }
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

    /// Set an exact canonical-name allowlist for executor tools. Unlike
    /// `AgentConfig.tools`, an empty list here explicitly denies every executor
    /// tool; coordinator-created ad-hoc workers use this to avoid inheriting the
    /// whole Session executor when their task requires no tools.
    pub fn with_executor_tool_allowlist(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        self.executor_tool_allowlist = Some(tools.into_iter().collect());
        self
    }

    /// Enable hook-based tool execution hooks.
    pub fn with_hook_registry(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hook_registry = Some(registry);
        self
    }

    /// Provide the optional append-only daily-log cache used to retain a
    /// bounded structured projection before context compaction summarizes it.
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
    /// the filesystem root down to `working_dir`, reading every regular,
    /// uniquely linked `AXOCOATL.md` it finds (root-most first,
    /// working-dir-most last — so deeper, more specific files appear later and
    /// can override broader org-wide ones). Directory traversal and file reads
    /// stay relative to opened directory handles so a Workspace symlink cannot
    /// make the host send an outside file to the model.
    ///
    /// This is the shared/versioned "team knowledge" layer — distinct from
    /// the per-user `core_memory` and `semantic_memory`. A file edit
    /// takes effect on the next actor spawn (session reopen).
    pub fn with_project_instructions(mut self, working_dir: &std::path::Path) -> Self {
        self.project_instructions = load_project_instructions(working_dir);
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
        if let Some(allowlist) = &self.executor_tool_allowlist {
            defs.retain(|definition| allowlist.contains(&definition.name));
        }
        // Agent-scoped recall tools are advertised alongside the executor's.
        // The set is deterministic per agent, so the tool list is stable turn to
        // turn. They're read-only, hence `Safe`.
        for (name, tool) in &self.recall_tools {
            defs.push(axocoatl_llm::ToolDefinition {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
                concurrency: tool.concurrency_policy(),
            });
        }
        // Core-memory edit tools — mutating, so advertised Exclusive.
        for (name, tool) in &self.core_memory_tools {
            defs.push(axocoatl_llm::ToolDefinition {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
                concurrency: tool.concurrency_policy(),
            });
        }
        defs
    }

    fn executor_tool_allowed(&self, name: &str) -> bool {
        self.executor_tool_allowlist
            .as_ref()
            .is_none_or(|allowlist| allowlist.contains(name))
    }

    /// True when the model emitted a call to one of this agent's recall tools.
    fn is_recall_tool(&self, name: &str) -> bool {
        self.recall_tools.iter().any(|(n, _)| n == name)
    }

    /// True when the model emitted a call to one of this agent's core-memory tools.
    fn is_core_memory_tool(&self, name: &str) -> bool {
        self.core_memory_tools.iter().any(|(n, _)| n == name)
    }

    /// Any agent-scoped tool the behavior services itself (recall + core memory),
    /// rather than the shared executor.
    fn is_behavior_tool(&self, name: &str) -> bool {
        self.is_recall_tool(name) || self.is_core_memory_tool(name)
    }

    fn behavior_tool(&self, name: &str) -> Option<Arc<dyn axocoatl_tools::BuiltinTool>> {
        self.recall_tools
            .iter()
            .chain(self.core_memory_tools.iter())
            .find(|(tool_name, _)| tool_name == name)
            .map(|(_, tool)| tool.clone())
    }

    fn behavior_tool_policy(&self, name: &str) -> Option<axocoatl_llm::ConcurrencyPolicy> {
        self.behavior_tool(name)
            .map(|tool| tool.concurrency_policy())
    }

    async fn execute_behavior_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
        let Some(tool) = self.behavior_tool(name) else {
            return Err(axocoatl_tools::ToolError::NotFound(name.to_string()));
        };
        let owned_name = name.to_string();
        match tokio::spawn(async move { tool.execute(arguments).await }).await {
            Ok(result) => result,
            Err(error) => Err(axocoatl_tools::ToolError::ExecutionFailed {
                tool: owned_name,
                reason: format!("Tool task panicked: {error}"),
            }),
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
            max_tokens: self.sampling.max_tokens,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            response_format: input
                .response_format_override
                .or(self.sampling.response_format),
            stop_sequences: Vec::new(),
            provider_options: input
                .reasoning_disabled
                .then(|| serde_json::json!({"reasoning_effort": "none"})),
            model_override: input.model_override.clone(),
        }
    }

    fn request_system_message(&self, system_override: Option<&str>) -> Option<ChatMessage> {
        let mem_context = self.memory_context();
        let effective_system = system_override.or(self.system_prompt.as_deref());
        match effective_system {
            Some(system) if mem_context.is_empty() => Some(ChatMessage::system(system)),
            Some(system) => Some(ChatMessage::system(format!("{system}\n\n{mem_context}"))),
            None if !mem_context.is_empty() => Some(ChatMessage::system(mem_context)),
            None => None,
        }
    }

    fn tool_definition_tokens(&self, tools: &[axocoatl_llm::ToolDefinition]) -> usize {
        tools
            .iter()
            .map(|definition| {
                let value = serde_json::to_value(definition)
                    .expect("ToolDefinition serialization is infallible");
                self.counter.count_tool_definition(&value)
            })
            .sum()
    }

    fn output_headroom_tokens(
        &self,
        request: &ChatRequest,
        capabilities: &axocoatl_llm::ProviderCapabilities,
    ) -> usize {
        request.max_tokens.unwrap_or(capabilities.max_output_tokens)
    }

    /// Reserve the locally estimated input plus the maximum completion the
    /// request permits before dispatch. For an Abort budget and a provider with
    /// no authoritative/default output limit, make the remaining safe output
    /// allowance explicit on the request so the remote call is still bounded.
    /// A provider can ultimately miscount input or ignore its output cap; that
    /// unavoidable remote overrun is surfaced by `record_provider_usage`.
    fn preflight_provider_spend(&self, request: &mut ChatRequest) -> Result<usize, AgentError> {
        let estimated_input = self.provider.count_tokens(request);
        let Some(tracker) = &self.tracker else {
            return Ok(estimated_input);
        };

        let provider_default_output =
            if request.max_tokens.is_none() && self.provider.model_constraints_known(request) {
                self.provider.capabilities_for(request).max_output_tokens
            } else {
                0
            };
        let output_reservation = request.max_tokens.unwrap_or_else(|| {
            if tracker.budget().overflow_policy != OverflowPolicy::Abort {
                return provider_default_output;
            }

            // An unset sampling maximum delegates to the provider default. For
            // an enforced budget, replace that open-ended default with the
            // largest completion that fits both local caps, additionally
            // bounded by an authoritative provider maximum when known.
            let execution_remaining = tracker
                .budget()
                .per_execution
                .saturating_sub(tracker.total_used());
            let call_allowance = tracker.budget().per_call.min(execution_remaining);
            let budget_safe_output = call_allowance.saturating_sub(estimated_input);
            let safe_output = if provider_default_output > 0 {
                budget_safe_output.min(provider_default_output)
            } else {
                budget_safe_output
            };
            if safe_output > 0 {
                request.max_tokens = Some(safe_output);
            }
            safe_output
        });

        // A chat call needs room for at least one output token. Treat an
        // unknown/default limit with no remaining allowance as a local budget
        // failure instead of dispatching an unbounded request.
        let requested = estimated_input.saturating_add(output_reservation);
        let checked_requested = if tracker.budget().overflow_policy == OverflowPolicy::Abort
            && output_reservation == 0
            && request.max_tokens.is_none()
        {
            requested.saturating_add(1)
        } else {
            requested
        };
        if let Err(BudgetError::WouldExceedBudget {
            current,
            requested,
            budget,
        }) = tracker.check_headroom(checked_requested)
        {
            match tracker.budget().overflow_policy {
                OverflowPolicy::Abort => {
                    return Err(AgentError::TokenBudgetExceeded {
                        used: current.saturating_add(requested),
                        budget,
                    });
                }
                OverflowPolicy::Warn => {
                    tracing::warn!(
                        current,
                        requested,
                        budget,
                        "Provider call would exceed token budget, continuing (warn policy)"
                    );
                }
            }
        }
        Ok(estimated_input)
    }

    /// Record provider-reported usage. Abort policy propagates an overrun
    /// immediately so no tool dispatch or follow-up provider call can continue
    /// on a silently exceeded budget; Warn remains explicitly advisory.
    fn record_provider_usage(
        &self,
        usage: &TokenUsageStats,
        usage_complete: bool,
    ) -> Result<(), AgentError> {
        if usage_complete {
            self.cumulative_token_usage.record_provider_response(usage);
            self.execution_usage.record_provider_response(usage);
        } else {
            // Retain the useful local numeric estimate while leaving the
            // activation explicitly incomplete.
            self.cumulative_token_usage.merge(usage);
            self.execution_usage.merge(usage);
        }
        let Some(tracker) = &self.tracker else {
            return Ok(());
        };
        let reported_total = usage.total();
        let tracked_output = usage
            .output_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        let per_call_overrun = (reported_total > tracker.budget().per_call).then_some(
            AgentError::TokenBudgetExceeded {
                used: reported_total,
                budget: tracker.budget().per_call,
            },
        );
        let recorded = tracker.record_usage(usage.input_tokens, tracked_output);
        match tracker.budget().overflow_policy {
            OverflowPolicy::Abort => {
                if let Some(error) = per_call_overrun {
                    return Err(error);
                }
                match recorded {
                    Ok(()) => Ok(()),
                    Err(BudgetError::ExecutionBudgetExceeded { used, budget }) => {
                        Err(AgentError::TokenBudgetExceeded { used, budget })
                    }
                    Err(BudgetError::WouldExceedBudget {
                        current,
                        requested,
                        budget,
                    }) => Err(AgentError::TokenBudgetExceeded {
                        used: current.saturating_add(requested),
                        budget,
                    }),
                }
            }
            OverflowPolicy::Warn => {
                if reported_total > tracker.budget().per_call {
                    tracing::warn!(
                        reported_total,
                        budget = tracker.budget().per_call,
                        "Provider-reported call usage exceeded per-call token budget (warn policy)"
                    );
                }
                if let Err(error) = recorded {
                    tracing::warn!(error = %error, "Provider-reported usage exceeded execution token budget (warn policy)");
                }
                Ok(())
            }
        }
    }

    fn cumulative_token_usage_snapshot(&self) -> TokenUsageStats {
        self.cumulative_token_usage.usage_snapshot()
    }

    fn cumulative_token_usage_measurement(&self) -> axocoatl_core::MeasuredTokenUsage {
        let usage = self.cumulative_token_usage_snapshot();
        match self.cumulative_token_usage.snapshot() {
            Some(_) => axocoatl_core::MeasuredTokenUsage::known(usage),
            None => axocoatl_core::MeasuredTokenUsage::lower_bound(usage),
        }
    }

    fn begin_provider_call(&self) {
        self.cumulative_token_usage.begin_provider_call();
        self.execution_usage.begin_provider_call();
    }

    fn begin_budgeted_operation(&mut self) {
        self.execution_usage.reset();
        self.tracker = self
            .token_budget
            .clone()
            .map(|budget| TokenTracker::new(budget, self.counter.clone()));
    }

    fn usage_changed(before: &TokenUsageStats, after: &TokenUsageStats) -> bool {
        before.input_tokens != after.input_tokens
            || before.output_tokens != after.output_tokens
            || before.reasoning_tokens != after.reasoning_tokens
    }

    fn measurement_changed(
        before: &axocoatl_core::MeasuredTokenUsage,
        after: &axocoatl_core::MeasuredTokenUsage,
    ) -> bool {
        before.complete != after.complete || Self::usage_changed(&before.usage, &after.usage)
    }

    /// A provider stream can report exact usage and then fail protocol
    /// validation (for example malformed tool arguments). Charge only usage
    /// that was actually received; an EOF/transport failure without a Usage
    /// event must not invent remote spend.
    fn account_reported_stream_usage_on_error(
        &self,
        usage: &TokenUsageStats,
        saw_usage: bool,
        error: AgentError,
    ) -> AgentError {
        if !saw_usage {
            return error;
        }
        match self.record_provider_usage(usage, true) {
            Ok(()) => error,
            Err(budget_error) => budget_error,
        }
    }

    async fn save_checkpoint_snapshot(
        &mut self,
        session_messages: Vec<StoredMessage>,
    ) -> Result<(), AgentError> {
        let Some(store) = self.checkpoint_store.clone() else {
            return Ok(());
        };
        self.checkpoint_version = self.checkpoint_version.saturating_add(1);
        let checkpoint = AgentCheckpoint {
            version: self.checkpoint_version,
            agent_id: self.agent_id.clone(),
            checkpoint_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            session_messages,
            cumulative_token_usage: self.cumulative_token_usage_snapshot(),
            cumulative_token_usage_known: self.cumulative_token_usage_measurement().complete,
            behavior_state: None,
        };
        store.save(&checkpoint).await.map_err(|error| {
            AgentError::Internal(format!(
                "checkpoint save for {} failed: {error}",
                self.agent_id
            ))
        })
    }

    fn message_segment_tokens(&self, messages: &[ChatMessage]) -> usize {
        let reply_priming = self.counter.count_messages(&[]);
        self.counter
            .count_messages(messages)
            .saturating_sub(reply_priming)
    }

    fn estimated_response_output_tokens(&self, response: &axocoatl_llm::ChatResponse) -> usize {
        axocoatl_llm::estimate_response_output_tokens(self.counter.as_ref(), response)
    }

    fn attachment_token_delta(
        &self,
        content: &str,
        attachments: &[axocoatl_core::AgentAttachment],
    ) -> usize {
        if attachments.is_empty() {
            return 0;
        }
        let mut request = ChatRequest::simple(content);
        let plain_tokens = self.counter.count_messages(&request.messages);
        attach_to_last_user_message(&mut request, attachments);
        self.counter
            .count_messages(&request.messages)
            .saturating_sub(plain_tokens)
    }

    fn request_constraints(
        &self,
        request: &ChatRequest,
    ) -> Option<(axocoatl_llm::ProviderCapabilities, usize)> {
        if !self.provider.model_constraints_known(request) {
            return None;
        }
        let capabilities = self.provider.capabilities_for(request);
        if capabilities.max_context_tokens == 0 {
            return None;
        }
        let target = (capabilities.max_context_tokens as f32
            * axocoatl_token::COMPRESSION_TRIGGER_PCT) as usize;
        Some((capabilities, target))
    }

    fn request_context_tokens(
        &self,
        request: &ChatRequest,
        capabilities: &axocoatl_llm::ProviderCapabilities,
    ) -> usize {
        self.counter
            .count_messages(&request.messages)
            .saturating_add(self.tool_definition_tokens(&request.tools))
            .saturating_add(self.output_headroom_tokens(request, capabilities))
    }

    fn ensure_request_fits_context(&self, request: &ChatRequest) -> Result<(), AgentError> {
        let Some((capabilities, limit)) = self.request_constraints(request) else {
            return Ok(());
        };
        let required = self.request_context_tokens(request, &capabilities);
        if required > limit {
            return Err(AgentError::ContextLimitExceeded { required, limit });
        }
        Ok(())
    }

    fn compression_error(error: axocoatl_token::CompressionError) -> AgentError {
        match error {
            axocoatl_token::CompressionError::ProtectedContextExceedsTarget {
                required_tokens,
                target_tokens,
            } => AgentError::ContextLimitExceeded {
                required: required_tokens,
                limit: target_tokens,
            },
            axocoatl_token::CompressionError::UnableToReachTarget {
                remaining_tokens,
                target_tokens,
            } => AgentError::ContextLimitExceeded {
                required: remaining_tokens,
                limit: target_tokens,
            },
            error => AgentError::Internal(format!("invalid compression boundary: {error}")),
        }
    }

    fn clear_synthetic_context_markers(messages: &mut [ChatMessage]) {
        for message in messages {
            if message.role == MessageRole::System
                && message.name.as_deref() == Some(axocoatl_token::SYNTHETIC_CONTEXT_MESSAGE_NAME)
            {
                message.name = None;
            }
        }
    }

    fn uncompressed_request_from_session(
        &self,
        system_override: Option<&str>,
        model_override: Option<String>,
    ) -> (ChatRequest, usize) {
        let mut messages = Vec::new();
        if let Some(system) = self.request_system_message(system_override) {
            messages.push(system);
        }
        let session_message_start = messages.len();
        messages.extend(self.session.as_chat_messages());

        (
            ChatRequest {
                messages,
                tools: self.tool_definitions(),
                max_tokens: self.sampling.max_tokens,
                temperature: self.sampling.temperature,
                top_p: self.sampling.top_p,
                response_format: self.sampling.response_format,
                stop_sequences: Vec::new(),
                provider_options: None,
                model_override: model_override.or_else(|| self.configured_model.clone()),
            },
            session_message_start,
        )
    }

    /// Build a ChatRequest from the current session memory.
    /// Includes system prompt + memory context (core blocks + recalled context) + full session history.
    /// `system_override` replaces the agent's configured system_prompt for
    /// this single call when `Some` — memory context still merges as usual.
    /// `model_override` swaps the model on the configured provider (same
    /// provider, same credentials — model name only).
    fn build_request_from_session(
        &self,
        system_override: Option<&str>,
        model_override: Option<String>,
        turn_start_session_index: usize,
        attachment_tokens: usize,
    ) -> Result<ChatRequest, AgentError> {
        let (mut request, session_message_start) =
            self.uncompressed_request_from_session(system_override, model_override);
        let Some((capabilities, _)) = self.request_constraints(&request) else {
            return Ok(request);
        };
        let fixed_tokens = self
            .tool_definition_tokens(&request.tools)
            .saturating_add(self.output_headroom_tokens(&request, &capabilities))
            .saturating_add(attachment_tokens);
        let pipeline = axocoatl_token::CompressionPipeline::new(
            self.counter.clone(),
            capabilities.max_context_tokens,
        );

        // Check if compression is needed (stages 1-2 only, pure computation)
        if pipeline.needs_compression(&request.messages, fixed_tokens) {
            tracing::info!(
                tokens = self
                    .counter
                    .count_messages(&request.messages)
                    .saturating_add(fixed_tokens),
                "Context compression triggered (session follow-up)"
            );
            request.messages = pipeline
                .compress_sync(
                    request.messages,
                    axocoatl_token::CompressionGuard::new(
                        session_message_start.saturating_add(turn_start_session_index),
                        fixed_tokens,
                    ),
                )
                .map_err(Self::compression_error)?
                .messages;
        }

        Self::clear_synthetic_context_markers(&mut request.messages);
        Ok(request)
    }

    /// Build a request from this input ALONE — the system override (or the
    /// configured prompt) + the input's history + its content, with the agent's
    /// sampling controls. No session, no memory context: a stateless call is a
    /// pure function of its input.
    fn build_stateless_request(&self, input: &AgentInput) -> ChatRequest {
        let mut messages = Vec::new();
        let effective_system = input
            .system_override
            .as_deref()
            .or(self.system_prompt.as_deref());
        if let Some(sys) = effective_system {
            messages.push(ChatMessage::system(sys));
        }
        for msg in &input.history {
            messages.push(msg.clone());
        }
        messages.push(ChatMessage::user(&input.content));

        ChatRequest {
            messages,
            // Stateless execution is intentionally one inference with no tool
            // loop. Advertising tools here would invite a valid ToolUse that
            // this pure path cannot execute or return honestly.
            tools: Vec::new(),
            max_tokens: self.sampling.max_tokens,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            response_format: input
                .response_format_override
                .or(self.sampling.response_format),
            stop_sequences: Vec::new(),
            provider_options: input
                .reasoning_disabled
                .then(|| serde_json::json!({"reasoning_effort": "none"})),
            model_override: input
                .model_override
                .clone()
                .or_else(|| self.configured_model.clone()),
        }
    }

    /// Stateless execution: a single inference from this input alone, with no
    /// reads or writes to the persistent session or checkpoint. A pure function
    /// of `(system, history, content)` — the right mode for per-request
    /// prompt/model variants and for scoring an agent over independent inputs.
    /// Single-shot by design: it does not run the (stateful) tool loop.
    async fn execute_stateless(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        if self.observe_cancellation() {
            return Ok(AgentOutput::text(""));
        }
        let mut request = self.build_stateless_request(&input);
        if !input.attachments.is_empty() {
            attach_to_last_user_message(&mut request, &input.attachments);
        }
        let (mut request, provider_tool_names) = Self::encode_provider_request(request)?;
        self.ensure_request_fits_context(&request)?;

        let est_input = if self.cancellation_requested() {
            self.provider.count_tokens(&request)
        } else {
            self.preflight_provider_spend(&mut request)?
        };
        let streamed = self.stream_chat(request, provider_tool_names).await?;
        let provider_cancelled = streamed.cancelled;
        let usage_complete = streamed.usage_complete;
        if provider_cancelled {
            self.active_run_cancelled = true;
        }
        let mut response = streamed.response;
        if response.usage.total() == 0 && (!provider_cancelled || !response.content.is_empty()) {
            response.usage =
                TokenUsageStats::new(est_input, self.estimated_response_output_tokens(&response));
        }
        if !provider_cancelled || response.usage.total() > 0 || !response.content.is_empty() {
            self.record_provider_usage(&response.usage, usage_complete)?;
        }
        Ok(AgentOutput {
            content: response.content,
            tool_calls: Vec::new(),
            token_usage: response.usage,
        })
    }

    /// Persistently compact only completed turns before the current User. The
    /// exact active User-to-tail suffix is atomic and the returned index is its
    /// new position after older-prefix compression.
    async fn compact_session(
        &mut self,
        turn_start_session_index: usize,
        system_override: Option<&str>,
        model_override: Option<String>,
        attachment_tokens: usize,
    ) -> Result<(usize, TokenUsageStats), AgentError> {
        let (request, session_message_start) =
            self.uncompressed_request_from_session(system_override, model_override);
        let Some((capabilities, target_threshold)) = self.request_constraints(&request) else {
            return Ok((turn_start_session_index, TokenUsageStats::default()));
        };
        let messages = self.session.as_chat_messages();
        let fixed_tokens = self
            .message_segment_tokens(&request.messages[..session_message_start])
            .saturating_add(self.tool_definition_tokens(&request.tools))
            .saturating_add(self.output_headroom_tokens(&request, &capabilities))
            .saturating_add(attachment_tokens);
        let pipeline = axocoatl_token::CompressionPipeline::new(
            self.counter.clone(),
            capabilities.max_context_tokens,
        );

        if !pipeline.needs_compression(&messages, fixed_tokens) {
            return Ok((turn_start_session_index, TokenUsageStats::default()));
        }
        let guard = axocoatl_token::CompressionGuard::new(turn_start_session_index, fixed_tokens);
        pipeline
            .validate_for_target(&messages, guard, target_threshold)
            .map_err(Self::compression_error)?;

        // When a Tier-2 archive is configured, its write is a prerequisite. If
        // it fails, Tier 1 remains untouched and no summarizer call is made.
        // Without Tier 2, the caller-owned canonical Session/Chat ledger still
        // owns the exact transcript, so compaction remains available.
        if let Some(daily_log) = &self.daily_log {
            let (archived_messages, omitted_messages) = structured_compaction_archive(&messages);
            let archive_truncated = omitted_messages > 0
                || archived_messages.iter().any(|message| {
                    message
                        .get("archive_truncated")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                });
            let entry = axocoatl_memory::LogEntry {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                entry_type: axocoatl_memory::LogEntryType::Conversation,
                content: serde_json::json!({
                    "reason": "context_compaction",
                    "target_threshold": target_threshold,
                    "messages": archived_messages,
                    "archive_truncated": archive_truncated,
                    "omitted_messages": omitted_messages,
                    "original_message_count": messages.len(),
                }),
            };
            daily_log.append(entry).await.map_err(|error| {
                AgentError::Internal(format!(
                    "failed to archive structured transcript before compaction: {error}"
                ))
            })?;
        } else {
            tracing::warn!(
                agent = %self.agent_id,
                "compacting actor context without optional Tier-2 archive; canonical Session/Chat history remains owned by its caller"
            );
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

        let summarizer = crate::summarizer::LlmSummarizer::new(
            self.provider.clone(),
            self.tracker.clone(),
            self.counter.clone(),
            request.model_override.clone(),
        )
        .with_usage_state(self.execution_usage.clone())
        .with_usage_state(self.cumulative_token_usage.clone());
        let result = pipeline
            .compress_to(
                messages,
                Some(&summarizer),
                housekeeping,
                target_threshold,
                guard,
            )
            .await;
        // The summarizer already charged the optional enforcement tracker and
        // both per-execution/lifetime usage states, including before an
        // empty/malformed summary error.
        let summarizer_usage = summarizer.usage_snapshot();
        let result = result.map_err(Self::compression_error)?;

        let counter = self.counter.clone();
        self.session
            .replace_with_chat_messages(&result.messages, |s| counter.count_text(s));

        // Single, overwritten-each-compaction hint pointing the agent at the
        // summary it can now see and telling it the detail behind it is
        // searchable. Only when recall is actually available.
        self.recall_toc_hint = if self.semantic_memory.is_some() {
            Some(
                "## Earlier context\nOlder turns in this conversation were summarized above to \
                 save space. Use `recall_search` to retrieve specifics that aren't in the summary."
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
        Ok((result.protected_suffix_start, summarizer_usage))
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
pub(crate) fn attach_to_last_user_message(
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
            // Always base64-inline images for vision-capable models. The
            // FileStore already resolved these bytes through its retained
            // directory capability before the Agent input was constructed.
            let data_uri = format!("data:{};base64,{}", a.mime, B64.encode(&a.bytes));
            image_parts.push(ContentPart::Image {
                url: data_uri,
                detail: ImageDetail::Auto,
            });
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
            match std::str::from_utf8(&a.bytes) {
                Ok(s) => {
                    text_with_files.push_str(&format!(
                        "\n\n<attachment name=\"{}\">\n{s}\n</attachment>",
                        a.name
                    ));
                }
                Err(_) => {
                    tracing::warn!(name = %a.name, mime = %a.mime, "non-image binary with no extracted text, skipping");
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

    fn cumulative_token_usage(&self) -> Option<TokenUsageStats> {
        Some(self.cumulative_token_usage_snapshot())
    }

    fn cumulative_token_usage_measurement(&self) -> Option<axocoatl_core::MeasuredTokenUsage> {
        Some(DefaultAgentBehavior::cumulative_token_usage_measurement(
            self,
        ))
    }

    fn last_execution_token_usage(&self) -> Option<TokenUsageStats> {
        self.execution_usage.snapshot()
    }

    fn last_execution_token_usage_measurement(&self) -> Option<axocoatl_core::MeasuredTokenUsage> {
        Some(self.execution_usage.measurement())
    }

    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError> {
        self.system_prompt = config.system_prompt.clone();
        self.configured_model = if config.model.is_empty() {
            None
        } else {
            Some(config.model.clone())
        };
        self.sampling = config.sampling.clone();
        self.agent_id = config.id.to_string();
        // Empty configured tools preserve the established behavior of
        // inheriting the execution path's baseline executor. A non-empty list
        // is an exact canonical allowlist. An explicit builder override (used
        // by ad-hoc coordinator workers) wins, including exact-empty.
        if self.executor_tool_allowlist.is_none() && !config.tools.is_empty() {
            self.executor_tool_allowlist = Some(config.tools.iter().cloned().collect());
        }

        self.cumulative_token_usage
            .set(TokenUsageStats::default(), true);

        // `per_execution` is per Execute activation, not actor lifetime. Keep
        // the policy here and create a fresh tracker at each operation entry.
        self.token_budget = config.token_budget.clone();
        self.tracker = None;

        // Restore from checkpoint if available
        if let Some(store) = &self.checkpoint_store {
            match store
                .load_latest(&config.id)
                .await
                .map_err(|e| AgentError::Internal(format!("Checkpoint restore: {e}")))?
            {
                Some(ckpt) => {
                    self.cumulative_token_usage.set(
                        ckpt.cumulative_token_usage.clone(),
                        ckpt.cumulative_token_usage_known,
                    );
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
        // One shared tracker covers compaction, the initial provider call, and
        // every tool follow-up in this activation. A later Execute starts with
        // fresh headroom; lifetime usage remains durable/reportable separately.
        self.begin_budgeted_operation();
        // A stateless call is a pure function of its input — no session, no
        // conversation or memory mutation. Provider usage is still persisted
        // in an accounting-only checkpoint against the unchanged actor
        // transcript. The right mode for per-request variants + eval.
        // `stateless` remains authoritative for older serialized callers that
        // predate ConversationMode.
        let conversation_mode = input.effective_conversation_mode();
        if conversation_mode == ConversationMode::Stateless {
            let canonical_messages = self.session.messages().to_vec();
            let usage_before = self.cumulative_token_usage_measurement();
            let mut outcome = self.execute_stateless(input).await;
            let usage_after = self.cumulative_token_usage_measurement();
            if Self::measurement_changed(&usage_before, &usage_after) {
                if let Err(checkpoint_error) =
                    self.save_checkpoint_snapshot(canonical_messages).await
                {
                    outcome = Err(match outcome {
                        Ok(_) => checkpoint_error,
                        Err(original_error) => AgentError::Internal(format!(
                            "{original_error}; additionally failed to persist incurred token usage: {checkpoint_error}"
                        )),
                    });
                }
            }
            return outcome;
        }
        // A stateful turn accepted by the actor remains part of its transcript
        // even if Stop wins before the provider starts. The status lives in the
        // caller's turn ledger; here we preserve the submitted user message and
        // then let the provider boundary below return an empty partial output.
        self.observe_cancellation();

        // Lightweight chats persist their transcript in ChatStore, not in this
        // configured agent's lifetime session/checkpoint. Seed a request-local
        // Tier-1 transcript, then run the *normal* execution path so streaming,
        // tools, hooks, spend tracking, and agent-scoped Tier-3/Tier-4 memory all
        // remain available. AgentActor serializes Execute messages, so swapping
        // the active transcript for the awaited call cannot overlap another
        // turn. Every Result path restores the canonical actor transcript below;
        // a panic terminates the actor, whose restart restores its last canonical
        // checkpoint rather than this call-local transcript.
        let canonical_session = if conversation_mode == ConversationMode::SuppliedHistory {
            let mut supplied_session = SessionMemory::new();
            let counter = self.counter.clone();
            supplied_session
                .replace_with_chat_messages(&input.history, |text| counter.count_text(text));
            Some(std::mem::replace(&mut self.session, supplied_session))
        } else {
            None
        };
        let persist_actor_session = canonical_session.is_none();
        let cumulative_before_execution = self.cumulative_token_usage_measurement();
        // A failed paid turn must never checkpoint an active User or partial
        // tool transaction. Start with the last complete actor-owned prefix;
        // after any successful prefix compaction this is refreshed below.
        let mut error_checkpoint_messages =
            persist_actor_session.then(|| self.session.messages().to_vec());

        let mut outcome = async {

        // Capture the boundary before appending the current User. Role inference
        // after compression is insufficient: tool-heavy turns can contain many
        // messages and older summaries can themselves use User-shaped markers.
        let mut turn_start_session_index = self.session.len();
        let mut compaction_usage = TokenUsageStats::default();
        let attachment_tokens = self.attachment_token_delta(&input.content, &input.attachments);

        // Append user input to the active Tier-1 transcript first. In the
        // default mode this is the actor-owned lifetime session; in supplied
        // mode it is the caller-owned, request-local chat transcript above.
        let input_tokens = self.counter.count_text(&input.content);
        self.session
            .append(MessageRole::User, &input.content, input_tokens);

        // Retrieve semantically-relevant memories for this turn (Tier 4).
        self.semantic_context = self.retrieve_semantic_context(&input.content);

        // Persistently summarize old context once the session has grown toward
        // the model's context window. The canonical Session/Chat ledger remains
        // authoritative; an optional daily-log cache receives a bounded
        // structured projection. No-op under the threshold or when the exact
        // request model has no known context constraint.
        if persist_actor_session && !self.cancellation_requested() {
            if let Some(control) = self.active_run_control.clone() {
                let compacted = tokio::select! {
                    biased;
                    _ = control.cancelled() => {
                        self.active_run_cancelled = true;
                        None
                    },
                    result = self.compact_session(
                        turn_start_session_index,
                        input.system_override.as_deref(),
                        input.model_override.clone(),
                        attachment_tokens,
                    ) => Some(result),
                };
                if let Some(result) = compacted {
                    let (new_boundary, usage) = result?;
                    turn_start_session_index = new_boundary;
                    compaction_usage.merge(&usage);
                }
            } else {
                let (new_boundary, usage) = self
                    .compact_session(
                        turn_start_session_index,
                        input.system_override.as_deref(),
                        input.model_override.clone(),
                        attachment_tokens,
                    )
                    .await?;
                turn_start_session_index = new_boundary;
                compaction_usage.merge(&usage);
            }
        }
        if persist_actor_session {
            error_checkpoint_messages = Some(
                self.session.messages()[..turn_start_session_index.min(self.session.len())]
                    .to_vec(),
            );
        }

        // Build from the active transcript. Supplied history was copied into a
        // request-local SessionMemory above; actor mode uses the durable session.
        // `input.system_override` (when Some, for example from the retained
        // lightweight-chat API) takes precedence over the agent's configured
        // system_prompt for this turn.
        let mut request = if self.active_run_cancelled {
            // Stop won while async compaction was in flight. Preserve the
            // unmodified transcript and let stream_chat's cancellation gate
            // return the normal empty partial result without re-running a
            // synchronous context gate on the intentionally uncompressed state.
            self.uncompressed_request_from_session(
                input.system_override.as_deref(),
                input.model_override.clone(),
            )
            .0
        } else {
            self.build_request_from_session(
                input.system_override.as_deref(),
                input.model_override.clone(),
                turn_start_session_index,
                attachment_tokens,
            )?
        };

        // If attachments came with this turn, upgrade the last (user) message
        // from a plain Text(content) into Parts(text + image parts) so the
        // provider can route them as vision content / inline blobs.
        if !self.active_run_cancelled && !input.attachments.is_empty() {
            attach_to_last_user_message(&mut request, &input.attachments);
        }
        let (mut request, provider_tool_names) = Self::encode_provider_request(request)?;
        if !self.active_run_cancelled {
            self.ensure_request_fits_context(&request)?;
        }

        // Make the LLM call — always streamed, so output is live by default.
        let est_input = if self.cancellation_requested() {
            self.provider.count_tokens(&request)
        } else {
            self.preflight_provider_spend(&mut request)?
        };
        let StreamChatResult {
            mut response,
            cancelled: provider_cancelled,
            usage_complete,
            provider_tool_names,
            provider_route,
        } = self.stream_chat(request, provider_tool_names).await?;
        if provider_cancelled {
            self.active_run_cancelled = true;
        }
        // Some providers' streams omit a final Usage event — fall back to a
        // local estimate so token accounting stays correct.
        if response.usage.total() == 0
            && (!provider_cancelled || !response.content.is_empty())
        {
            response.usage = TokenUsageStats::new(
                est_input,
                self.estimated_response_output_tokens(&response),
            );
        }
        let mut execution_usage = compaction_usage;
        execution_usage.merge(&response.usage);
        // The provider has completed and this usage is now incurred. Account
        // it before any compatibility recovery or protocol validation can
        // reject the response; the actor error path durably checkpoints the
        // updated cumulative total without retaining an incomplete turn.
        if !provider_cancelled || response.usage.total() > 0 || !response.content.is_empty() {
            self.record_provider_usage(&response.usage, usage_complete)?;
        }

        // Ollama compatibility fallback: some small locally served models
        // intermittently emit tool calls as JSON in the message text rather
        // than via the structured tool_calls channel.
        // When `response.tool_calls` is empty we scan `response.content`
        // for top-level JSON objects of shape `{ "tool_name": { args } }`
        // where `tool_name` matches a registered tool, and adopt them.
        // No-op for any model that uses the structured channel
        // correctly — `tool_calls` is non-empty so the block is skipped.
        // The selected route, not the wrapper's primary identity, governs both
        // eligibility and durable provenance when fallback chose Ollama.
        let effective_response_provider = provider_route
            .get(axocoatl_llm::TOOL_METADATA_ROUTE_PROVIDER)
            .filter(|provider| !provider.is_empty())
            .cloned()
            .or_else(|| (!response.provider.is_empty()).then(|| response.provider.clone()))
            .unwrap_or_else(|| self.provider.provider_id().to_string());
        if !self.active_run_cancelled
            && response.tool_calls.is_empty()
            && supports_text_tool_recovery(&effective_response_provider)
        {
            let mut fallback = Vec::new();
            for v in extract_top_level_json(&response.content)? {
                let Some(obj) = v.as_object() else { continue };
                if obj.len() != 1 {
                    continue;
                }
                let (key, value) = obj.iter().next().unwrap();
                let Some(internal_name) = provider_tool_names.decode_advertised_name(key) else {
                    continue;
                };
                if !value.is_object() {
                    continue;
                }
                if fallback.len() >= MAX_PROVIDER_TOOL_CALLS {
                    return Err(AgentError::Provider(format!(
                        "provider text recovery exceeded the portable {MAX_PROVIDER_TOOL_CALLS}-call limit"
                    )));
                }
                let mut provider_metadata =
                    axocoatl_llm::provider_tool_metadata(&effective_response_provider);
                merge_provider_metadata(
                    &mut provider_metadata,
                    provider_route.clone(),
                    "provider text-recovery metadata conflicts with its selected route",
                )?;
                fallback.push(ToolCall {
                    id: text_tool_recovery_id(fallback.len())?,
                    name: internal_name.to_string(),
                    arguments: value.clone(),
                    provider_metadata,
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

        // Tool execution loop: if LLM returns tool calls, execute them and continue
        let mut tool_records = Vec::new();
        let mut tool_activity_count = 0_usize;
        let mut tool_error_count = 0_usize;
        let mut unresolved_tool_count = 0_usize;
        let mut last_tool_error: Option<(String, String)> = None;
        let mut loop_count = 0;
        const MAX_TOOL_LOOPS: usize = 10;

        while !self.active_run_cancelled
            && !response.tool_calls.is_empty()
            && loop_count < MAX_TOOL_LOOPS
        {
            // No assistant tool-call turn has been recorded yet, so stopping at
            // this boundary cannot leave orphaned tool messages in history.
            if self.observe_cancellation() {
                response.tool_calls.clear();
                break;
            }
            loop_count += 1;
            tool_activity_count = tool_activity_count.saturating_add(response.tool_calls.len());

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
                let mut approved_call_indexes = Vec::new();
                let mut surfaced_calls = Vec::new();
                // Denials are decisions made during Phase 1, but their result
                // events/history must join the same original-index merge as
                // dispatched results. Otherwise a denied middle call is stored
                // before earlier parallel calls complete (B,A,C instead of
                // A,B,C), which makes id-less provider replay ambiguous.
                let mut deferred_results: Vec<(
                    usize,
                    axocoatl_tools::ToolResult,
                    bool,
                )> = Vec::new();
                for (call_index, tc) in response.tool_calls.iter().enumerate() {
                    if self.cancellation_requested() {
                        self.active_run_cancelled = true;
                        approved_calls.extend(response.tool_calls[call_index..].iter().cloned());
                        approved_call_indexes.extend(call_index..response.tool_calls.len());
                        surfaced_calls.extend(
                            response.tool_calls[call_index..]
                                .iter()
                                .cloned()
                                .enumerate()
                                .map(|(offset, call)| (call_index + offset, call)),
                        );
                        break;
                    }
                    if !self.is_behavior_tool(&tc.name)
                        && !self.executor_tool_allowed(&tc.name)
                    {
                        // Defense in depth: request-time advertisement already
                        // excludes this name, but never let an unexpected model
                        // call reach policy hooks or the dispatcher.
                        surfaced_calls.push((call_index, tc.clone()));
                        deferred_results.push((
                            call_index,
                            axocoatl_tools::ToolResult {
                                seq: call_index,
                                tool_call: tc.clone(),
                                result: Err(axocoatl_tools::ToolError::NotFound(
                                    tc.name.clone(),
                                )),
                            },
                            false,
                        ));
                        continue;
                    }
                    if let Some(hooks) = &self.hook_registry {
                        let (action, transformed_args) = hooks
                            .run_pre_hooks(&tc.name, &self.agent_id, tc.arguments.clone())
                            .await;
                        match action {
                            axocoatl_tools::HookAction::Deny { reason } => {
                                tracing::warn!(tool = %tc.name, reason = %reason, "Tool call denied by hook");
                                surfaced_calls.push((call_index, tc.clone()));
                                deferred_results.push((
                                    call_index,
                                    axocoatl_tools::ToolResult {
                                        seq: call_index,
                                        tool_call: tc.clone(),
                                        result: Ok(serde_json::json!({"error": reason})),
                                    },
                                    false,
                                ));
                                continue;
                            }
                            _ => {
                                // Allow or Transform — use (possibly transformed) arguments
                                let approved = axocoatl_llm::ToolCall {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    arguments: transformed_args,
                                    provider_metadata: tc.provider_metadata.clone(),
                                };
                                surfaced_calls.push((call_index, approved.clone()));
                                approved_calls.push(approved);
                                approved_call_indexes.push(call_index);
                            }
                        }
                    } else {
                        surfaced_calls.push((call_index, tc.clone()));
                        approved_calls.push(tc.clone());
                        approved_call_indexes.push(call_index);
                    }
                }

                // Observe cancellation here, before any approved tool starts.
                // Phase 2 converts every approved call into an indexed
                // cancellation result and Phase 3 closes the full assistant
                // group in provider order together with any denials.
                self.observe_cancellation();

                // Surface every provider call exactly once, after all pre-hook
                // decisions, in original provider order. FIFO consumers can
                // then correlate even id-less same-name calls without relying
                // on a later index-based repair.
                surfaced_calls.sort_by_key(|(provider_call_index, _)| *provider_call_index);
                for (provider_call_index, call) in &surfaced_calls {
                    self.emit_stream(crate::behavior::AgentStreamChunk::ToolCallStarted {
                        source_agent: None,
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        provider_arguments: response.tool_calls[*provider_call_index].arguments.clone(),
                        provider_metadata: call.provider_metadata.clone(),
                        assistant_content: (*provider_call_index == 0)
                            .then(|| response.content.clone()),
                        provider_response_group: loop_count as u64,
                        provider_call_index: *provider_call_index,
                        provider_call_count: response.tool_calls.len(),
                    });
                }

                // Phase 2: plan across BOTH agent-scoped and executor backends.
                // If any approved call is Exclusive, the entire provider group
                // runs sequentially in original order; partitioning by backend
                // first would otherwise reorder a core-memory mutation ahead of
                // an earlier repository/shell mutation.
                let mut indexed = deferred_results;
                let has_exclusive = approved_calls.iter().any(|call| {
                    self.behavior_tool_policy(&call.name)
                        .or_else(|| {
                            executor
                                .as_ref()
                                .and_then(|exec| exec.get_concurrency_policy(&call.name))
                        })
                        .unwrap_or(axocoatl_llm::ConcurrencyPolicy::Exclusive)
                        == axocoatl_llm::ConcurrencyPolicy::Exclusive
                });

                if has_exclusive {
                    for (call, provider_call_index) in
                        approved_calls.iter().zip(&approved_call_indexes)
                    {
                        let i = *provider_call_index;
                        let (result, run_post_hooks) = if self.cancellation_requested() {
                            self.active_run_cancelled = true;
                            (
                                Err(axocoatl_tools::ToolError::ExecutionFailed {
                                    tool: call.name.clone(),
                                    reason: "cancelled before tool execution".to_string(),
                                }),
                                false,
                            )
                        } else if self.is_behavior_tool(&call.name) {
                            (
                                self.execute_behavior_tool(
                                    &call.name,
                                    call.arguments.clone(),
                                )
                                .await,
                                true,
                            )
                        } else if let Some(exec) = &executor {
                            let policy = exec
                                .get_concurrency_policy(&call.name)
                                .unwrap_or(axocoatl_llm::ConcurrencyPolicy::Exclusive);
                            let mut results = ConcurrentToolDispatcher::dispatch(
                                exec,
                                std::slice::from_ref(call),
                                |_| policy,
                            )
                            .await;
                            let result = results
                                .pop()
                                .expect("one submitted tool call produces one result")
                                .result;
                            (result, true)
                        } else {
                            (
                                Err(axocoatl_tools::ToolError::NotFound(call.name.clone())),
                                true,
                            )
                        };
                        indexed.push((
                            i,
                            axocoatl_tools::ToolResult {
                                seq: i,
                                tool_call: call.clone(),
                                result,
                            },
                            run_post_hooks,
                        ));
                    }
                } else {
                    let mut exec_calls: Vec<(usize, axocoatl_llm::ToolCall)> = Vec::new();
                    for (call, provider_call_index) in
                        approved_calls.iter().zip(&approved_call_indexes)
                    {
                        let i = *provider_call_index;
                        if self.cancellation_requested() {
                            self.active_run_cancelled = true;
                            indexed.push((
                                i,
                                axocoatl_tools::ToolResult {
                                    seq: i,
                                    tool_call: call.clone(),
                                    result: Err(axocoatl_tools::ToolError::ExecutionFailed {
                                        tool: call.name.clone(),
                                        reason: "cancelled before tool execution".to_string(),
                                    }),
                                },
                                false,
                            ));
                        } else if self.is_behavior_tool(&call.name) {
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
                                true,
                            ));
                        } else {
                            exec_calls.push((i, call.clone()));
                        }
                    }
                    if let Some(exec) = &executor {
                        if self.cancellation_requested() {
                            self.active_run_cancelled = true;
                            for (i, call) in &exec_calls {
                                indexed.push((
                                    *i,
                                    axocoatl_tools::ToolResult {
                                        seq: *i,
                                        tool_call: call.clone(),
                                        result: Err(
                                            axocoatl_tools::ToolError::ExecutionFailed {
                                                tool: call.name.clone(),
                                                reason: "cancelled before tool execution"
                                                    .to_string(),
                                            },
                                        ),
                                    },
                                    false,
                                ));
                            }
                        } else {
                            // The Safe/Ordered batch is now started. Do not race
                            // cancellation against this await: every tool may
                            // already have produced a side effect and must reach
                            // its boundary.
                            let calls: Vec<axocoatl_llm::ToolCall> =
                                exec_calls.iter().map(|(_, call)| call.clone()).collect();
                            let exec_results = ConcurrentToolDispatcher::dispatch(
                                exec,
                                &calls,
                                |name| {
                                    exec.get_concurrency_policy(name).unwrap_or(
                                        axocoatl_llm::ConcurrencyPolicy::Exclusive,
                                    )
                                },
                            )
                            .await;
                            for ((orig_i, _), result) in exec_calls.iter().zip(exec_results) {
                                indexed.push((*orig_i, result, true));
                            }
                        }
                    } else {
                        for (i, call) in &exec_calls {
                            indexed.push((
                                *i,
                                axocoatl_tools::ToolResult {
                                    seq: *i,
                                    tool_call: call.clone(),
                                    result: Err(axocoatl_tools::ToolError::NotFound(
                                        call.name.clone(),
                                    )),
                                },
                                true,
                            ));
                        }
                    }
                }
                indexed.sort_by_key(|(i, _, _)| *i);
                // Phase 3: Run post-hooks and record results
                for (_, tool_result, run_post_hooks) in indexed {
                    let tc = &tool_result.tool_call;
                    let mut result = tool_result
                        .result
                        .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));

                    if run_post_hooks {
                        if let Some(hooks) = &self.hook_registry {
                            result = hooks.run_post_hooks(&tc.name, &self.agent_id, result).await;
                        }
                    }

                    tool_records.push(axocoatl_core::ToolCallRecord {
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        result: Some(result.clone()),
                    });

                    let is_error = result.get("error").is_some();
                    if is_error {
                        tool_error_count = tool_error_count.saturating_add(1);
                        let detail = result
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| result.to_string());
                        last_tool_error = Some((tc.name.clone(), detail));
                    }

                    self.emit_stream(crate::behavior::AgentStreamChunk::ToolCallResult {
                        source_agent: None,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        result: result.clone(),
                        is_error,
                    });

                    let result_str = serde_json::to_string(&result).unwrap_or_default();
                    let tool_tokens = self.counter.count_text(&result_str);
                    self.session
                        .append_tool_result(&tc.name, &tc.id, &result_str, tool_tokens);
                }

                // Once dispatch begins, every started tool and post-hook is
                // awaited and recorded. Cancellation is observed only here, at
                // the next side-effect-safe boundary, before another LLM call.
                if self.observe_cancellation() {
                    response.tool_calls.clear();
                    break;
                }

                // Make follow-up LLM call with tool results — streamed too.
                // Same overrides apply as the original turn.
                let mut followup = self.build_request_from_session(
                    input.system_override.as_deref(),
                    input.model_override.clone(),
                    turn_start_session_index,
                    attachment_tokens,
                )?;
                if !input.attachments.is_empty() {
                    attach_to_last_user_message(&mut followup, &input.attachments);
                }
                let (mut followup, provider_tool_names) =
                    Self::encode_provider_request(followup)?;
                self.ensure_request_fits_context(&followup)?;
                let est = self.preflight_provider_spend(&mut followup)?;
                let streamed = self.stream_chat(followup, provider_tool_names).await?;
                let provider_cancelled = streamed.cancelled;
                let usage_complete = streamed.usage_complete;
                if provider_cancelled {
                    self.active_run_cancelled = true;
                }
                response = streamed.response;
                if response.usage.total() == 0
                    && (!provider_cancelled || !response.content.is_empty())
                {
                    response.usage = TokenUsageStats::new(
                        est,
                        self.estimated_response_output_tokens(&response),
                    );
                }
                execution_usage.merge(&response.usage);
                if !provider_cancelled || response.usage.total() > 0 || !response.content.is_empty()
                {
                    self.record_provider_usage(&response.usage, usage_complete)?;
                }
            } else {
                // No tool executor — record calls but don't execute
                unresolved_tool_count = unresolved_tool_count
                    .saturating_add(response.tool_calls.len());
                if let Some(call) = response.tool_calls.last() {
                    last_tool_error = Some((
                        call.name.clone(),
                        "no executor was available for the requested tool".to_string(),
                    ));
                }
                for (provider_call_index, tc) in response.tool_calls.iter().enumerate() {
                    let result = serde_json::json!({
                        "error": "no executor was available for the requested tool"
                    });
                    self.emit_stream(crate::behavior::AgentStreamChunk::ToolCallStarted {
                        source_agent: None,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        provider_arguments: tc.arguments.clone(),
                        provider_metadata: tc.provider_metadata.clone(),
                        assistant_content: (provider_call_index == 0)
                            .then(|| response.content.clone()),
                        provider_response_group: loop_count as u64,
                        provider_call_index,
                        provider_call_count: response.tool_calls.len(),
                    });
                    self.emit_stream(crate::behavior::AgentStreamChunk::ToolCallResult {
                        source_agent: None,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        result: result.clone(),
                        is_error: true,
                    });
                    tool_records.push(axocoatl_core::ToolCallRecord {
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        result: Some(result),
                    });
                }
                break;
            }
        }

        if !self.active_run_cancelled
            && loop_count == MAX_TOOL_LOOPS
            && !response.tool_calls.is_empty()
        {
            let mut pending = response
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>();
            pending.sort_unstable();
            pending.dedup();
            let pending = if pending.is_empty() {
                "unnamed tool".to_string()
            } else {
                pending.join(", ")
            };
            return Err(AgentError::ToolFailed {
                tool: "agent tool loop".to_string(),
                reason: format!(
                    "the model still requested tools after the safety limit of {MAX_TOOL_LOOPS} rounds (pending: {pending}); those pending calls were not executed. Retry with a more capable model or narrow the task"
                ),
            });
        }

        if !self.active_run_cancelled
            && tool_activity_count > 0
            && (tool_error_count > 0 || unresolved_tool_count > 0)
            && response.content.trim().is_empty()
        {
            let reason = match last_tool_error {
                Some((tool, detail)) => format!(
                    "the model returned no final answer after {tool_activity_count} tool calls, including {tool_error_count} failed and {unresolved_tool_count} unresolved calls; the last failure was from '{tool}': {detail}. Retry with a more capable model or narrow the task"
                ),
                None => format!(
                    "the model returned no final answer after {tool_activity_count} tool calls, including {tool_error_count} failed and {unresolved_tool_count} unresolved calls. Retry with a more capable model or narrow the task"
                ),
            };
            return Err(AgentError::ToolFailed {
                tool: "agent tool loop".to_string(),
                reason,
            });
        }

        // Track assistant response in session
        let output_tokens = self.counter.count_text(&response.content);
        if !self.active_run_cancelled || !response.content.is_empty() {
            self.session
                .append(MessageRole::Assistant, &response.content, output_tokens);
        }

        // Persist this exchange to semantic memory for future cross-session
        // recall. Best-effort — a store failure is logged, never fatal.
        if !self.active_run_cancelled {
            if let Some(mem) = &self.semantic_memory {
            let exchange = format!("User: {}\nAssistant: {}", input.content, response.content);
            if let Err(e) = mem.store(&exchange, serde_json::json!({ "agent": self.agent_id })) {
                tracing::debug!(error = %e, "semantic memory store failed");
            }
            }
        }

        // The outer accounting boundary checkpoints every completed paid
        // activation. Preserve the older interval checkpoint behavior only
        // for a provider-free/cancelled actor-session mutation.
        if persist_actor_session
            && !Self::measurement_changed(
                &cumulative_before_execution,
                &self.cumulative_token_usage_measurement(),
            )
        {
            if let Some(store) = &self.checkpoint_store {
                if store.should_checkpoint(self.session.messages().len()) {
                    let session_messages = self.session.messages().to_vec();
                    self.save_checkpoint_snapshot(session_messages).await?;
                }
            }
        }

        Ok(AgentOutput {
            content: response.content,
            tool_calls: tool_records,
            token_usage: execution_usage,
        })
        }
        .await;

        let cumulative_after_execution = self.cumulative_token_usage_measurement();
        if Self::measurement_changed(&cumulative_before_execution, &cumulative_after_execution) {
            // Paid usage is durable even when semantic/protocol validation
            // fails. Actor-session errors retain only the last complete prefix;
            // request-local modes write accounting against the unchanged
            // canonical actor transcript.
            let checkpoint_messages = if persist_actor_session {
                if outcome.is_ok() {
                    self.session.messages().to_vec()
                } else {
                    error_checkpoint_messages.take().unwrap_or_default()
                }
            } else {
                canonical_session
                    .as_ref()
                    .map(|session| session.messages().to_vec())
                    .unwrap_or_default()
            };
            if let Err(checkpoint_error) = self.save_checkpoint_snapshot(checkpoint_messages).await
            {
                outcome = Err(match outcome {
                    Ok(_) => checkpoint_error,
                    Err(original_error) => AgentError::Internal(format!(
                        "{original_error}; additionally failed to persist incurred token usage: {checkpoint_error}"
                    )),
                });
            }
        }

        if let Some(canonical_session) = canonical_session {
            self.session = canonical_session;
        }

        outcome
    }

    async fn execute_controlled(
        &mut self,
        input: AgentInput,
        control: AgentRunControl,
    ) -> Result<AgentRunOutcome, AgentError> {
        debug_assert!(self.active_run_control.is_none());
        let run_id = control.id().clone();
        self.active_run_cancelled = false;
        self.active_run_control = Some(control);

        let result = self.execute(input).await;
        let cancelled = self.active_run_cancelled;
        self.active_run_control = None;
        self.active_run_cancelled = false;

        result.map(|output| {
            if cancelled {
                AgentRunOutcome::Cancelled {
                    run_id,
                    partial_output: output,
                }
            } else {
                AgentRunOutcome::Completed(output)
            }
        })
    }

    /// Background "sleep-time" consolidation: an LLM memory-manager pass that
    /// promotes durable facts from recent Tier-4 activity into the curated core
    /// blocks and tidies them. Promotion-only — it reads Tier 4, never evicts it.
    async fn on_consolidate(&mut self) -> Result<crate::behavior::ConsolidationReport, AgentError> {
        // Consolidation is its own paid activation and must not inherit the
        // previous turn's tracker or consume the next turn's headroom.
        self.begin_budgeted_operation();
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
        let mut request = ChatRequest {
            messages: vec![ChatMessage::system(system), ChatMessage::user(&user)],
            tools: vec![],
            max_tokens: Some(800),
            temperature: Some(0.0),
            top_p: None,
            response_format: None,
            stop_sequences: Vec::new(),
            provider_options: None,
            // Honor the agent's configured model on OpenAI-compatible servers.
            model_override: self.configured_model.clone(),
        };
        let estimated_input = self.preflight_provider_spend(&mut request)?;
        self.begin_provider_call();
        let mut response = match self.provider.chat(request).await {
            Ok(response) => response,
            Err(error) => {
                let provider_error = AgentError::Provider(error.to_string());
                return match self
                    .save_checkpoint_snapshot(self.session.messages().to_vec())
                    .await
                {
                    Ok(()) => Err(provider_error),
                    Err(checkpoint_error) => Err(AgentError::Internal(format!(
                        "{provider_error}; additionally failed to persist unknown consolidation usage: {checkpoint_error}"
                    ))),
                };
            }
        };
        if response.usage.total() == 0 {
            response.usage =
                TokenUsageStats::new(estimated_input, self.counter.count_text(&response.content));
        }
        let tokens_used = response.usage.total();
        let usage_result = self.record_provider_usage(&response.usage, true);
        // Consolidation is an explicit paid lifecycle call outside `execute`,
        // so persist its cumulative usage immediately against the complete
        // current transcript. This also preserves an incurred overrun before
        // returning the budget error.
        let checkpoint_result = self
            .save_checkpoint_snapshot(self.session.messages().to_vec())
            .await;
        if let Err(error) = usage_result {
            checkpoint_result?;
            return Err(error);
        }
        checkpoint_result?;

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

        // Stopping an actor is also the cancellation and replacement path. It
        // must not start fresh provider work or mutate durable memory after the
        // caller has requested Stop. Consolidation remains an explicit daemon
        // lifecycle action through `on_consolidate`.
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
fn extract_top_level_json(text: &str) -> Result<Vec<serde_json::Value>, AgentError> {
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
                if out.len() >= MAX_TEXT_JSON_CANDIDATES {
                    return Err(AgentError::Provider(format!(
                        "provider text contained more than {MAX_TEXT_JSON_CANDIDATES} top-level JSON candidates"
                    )));
                }
                out.push(v);
            }
            i = j + 1;
        } else {
            // Unbalanced — stop, there can't be another well-formed top-level
            // object after an unclosed one starting here.
            break;
        }
    }
    Ok(out)
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

    #[test]
    fn project_instructions_load_regular_ancestor_files_root_to_leaf() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("team/project");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(root.path().join("AXOCOATL.md"), "organization rule").unwrap();
        std::fs::create_dir_all(root.path().join("team")).unwrap();
        std::fs::write(root.path().join("team/AXOCOATL.md"), "repository rule").unwrap();
        std::fs::write(workspace.join("AXOCOATL.md"), "project rule").unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();

        let behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_project_instructions(&workspace);
        let context = behavior.memory_context();
        let organization = context.find("organization rule").unwrap();
        let repository = context.find("repository rule").unwrap();
        let project = context.find("project rule").unwrap();
        assert!(organization < repository);
        assert!(repository < project);
    }

    #[cfg(unix)]
    #[test]
    fn project_instructions_never_follow_workspace_symlink_outside() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(root.path().join("AXOCOATL.md"), "trusted parent rule").unwrap();
        let outside = root.path().join("outside-secret");
        std::fs::write(&outside, "HOST SECRET MUST NOT ENTER PROMPT").unwrap();
        symlink(&outside, workspace.join("AXOCOATL.md")).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();

        let behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_project_instructions(&workspace);
        let context = behavior.memory_context();
        assert!(context.contains("trusted parent rule"));
        assert!(!context.contains("HOST SECRET MUST NOT ENTER PROMPT"));
        assert!(!context.contains(&workspace.join("AXOCOATL.md").display().to_string()));
    }

    #[test]
    fn project_instructions_skip_oversized_regular_file_and_continue_deeper() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("team/project");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut oversized = b"OVERSIZED INSTRUCTION MUST NOT ENTER PROMPT\n".to_vec();
        oversized.resize(PROJECT_INSTRUCTION_FILE_MAX_BYTES + 1, b'x');
        std::fs::write(root.path().join("AXOCOATL.md"), oversized).unwrap();
        std::fs::write(workspace.join("AXOCOATL.md"), "bounded project rule").unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();

        let behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_project_instructions(&workspace);
        let context = behavior.memory_context();
        assert!(context.contains("bounded project rule"));
        assert!(!context.contains("OVERSIZED INSTRUCTION MUST NOT ENTER PROMPT"));
        assert!(!context.contains(&root.path().join("AXOCOATL.md").display().to_string()));
    }

    #[test]
    fn project_instructions_reject_oversized_sparse_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(root.path().join("AXOCOATL.md"), "trusted parent rule").unwrap();

        let sparse_path = workspace.join("AXOCOATL.md");
        let mut sparse = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&sparse_path)
            .unwrap();
        std::io::Write::write_all(&mut sparse, b"SPARSE INSTRUCTION MUST NOT ENTER PROMPT")
            .unwrap();
        sparse
            .set_len(PROJECT_INSTRUCTION_FILE_MAX_BYTES as u64 + 1)
            .unwrap();
        drop(sparse);
        let workspace = std::fs::canonicalize(workspace).unwrap();

        let behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_project_instructions(&workspace);
        let context = behavior.memory_context();
        assert!(context.contains("trusted parent rule"));
        assert!(!context.contains("SPARSE INSTRUCTION MUST NOT ENTER PROMPT"));
        assert!(!context.contains(&sparse_path.display().to_string()));
    }

    #[test]
    fn project_instructions_enforce_aggregate_byte_limit() {
        let root = tempfile::tempdir().unwrap();
        let mut workspace = root.path().to_path_buf();
        for index in 0..5 {
            std::fs::create_dir_all(&workspace).unwrap();
            let marker = format!("aggregate-instruction-{index}:");
            let mut body = marker.as_bytes().to_vec();
            body.resize(
                PROJECT_INSTRUCTION_FILE_MAX_BYTES,
                b'a' + u8::try_from(index).unwrap(),
            );
            std::fs::write(workspace.join("AXOCOATL.md"), body).unwrap();
            workspace = workspace.join(format!("level-{index}"));
        }
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();

        let behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_project_instructions(&workspace);
        let context = behavior.memory_context();
        assert!(context.contains("aggregate-instruction-0:"));
        assert!(context.contains("aggregate-instruction-3:"));
        assert!(!context.contains("aggregate-instruction-4:"));
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
                    Ok(StreamEvent::Usage(TokenUsageStats::new(11, 3))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "final answer".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(17, 5))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct ParallelPanicThenTextLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ParallelPanicThenTextLlm {
        fn provider_id(&self) -> &str {
            "parallel-panic"
        }

        fn model_id(&self) -> &str {
            "parallel-panic-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unimplemented!("parallel panic seam uses chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.captured.lock().unwrap().push(request);
            let round = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if round == 0 {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "call-a".to_string(),
                        name: Some("panic_tool".to_string()),
                        args_delta: "{}".to_string(),
                    }),
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(1),
                        id: "call-b".to_string(),
                        name: Some("echo".to_string()),
                        args_delta: r#"{"text":"ok"}"#.to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "parallel panic handled".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    /// Models a direct Ollama-like provider that emits a valid tool call in
    /// text instead of the structured stream channel on its first round.
    struct TextToolThenTextLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for TextToolThenTextLlm {
        fn provider_id(&self) -> &str {
            "ollama"
        }

        fn model_id(&self) -> &str {
            "ollama-like-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unimplemented!("text recovery seam uses chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.captured.lock().unwrap().push(request);
            let round = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let content = if round == 0 {
                r#"{"echo":{"text":"hi"}}"#
            } else {
                "text recovery complete"
            };
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    delta: content.to_string(),
                }),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    struct TextJsonOnlyLlm {
        provider: &'static str,
        content: String,
        reported_usage: Option<TokenUsageStats>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for TextJsonOnlyLlm {
        fn provider_id(&self) -> &str {
            self.provider
        }

        fn model_id(&self) -> &str {
            "text-json-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unimplemented!("text JSON seam uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut events = vec![Ok(StreamEvent::TextDelta {
                delta: self.content.clone(),
            })];
            if let Some(usage) = &self.reported_usage {
                events.push(Ok(StreamEvent::Usage(usage.clone())));
            }
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }));
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct ManyStructuredCallsLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ManyStructuredCallsLlm {
        fn provider_id(&self) -> &str {
            "many-structured-calls"
        }

        fn model_id(&self) -> &str {
            "many-structured-calls-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("structured accumulation seam uses chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let name = request
                .tools
                .first()
                .map(|tool| tool.name.clone())
                .unwrap_or_else(|| "echo".to_string());
            let mut events = (0..=MAX_PROVIDER_TOOL_CALLS)
                .map(|index| {
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(index),
                        id: format!("call-{index}"),
                        name: Some(name.clone()),
                        args_delta: "{}".to_string(),
                    })
                })
                .collect::<Vec<_>>();
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }));
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct RateLimitedPrimaryLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for RateLimitedPrimaryLlm {
        fn provider_id(&self) -> &str {
            "openai"
        }

        fn model_id(&self) -> &str {
            "primary-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("fallback recovery seam uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(ProviderError::RateLimited {
                provider: "openai".to_string(),
                retry_after_secs: None,
            })
        }
    }

    /// Uses the name advertised on the request rather than knowing Axocoatl's
    /// canonical executor key. This models a real provider round trip and lets
    /// the seam test prove alias reversal before hooks and dispatch.
    struct ProviderAliasThenTextLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ProviderAliasThenTextLlm {
        fn provider_id(&self) -> &str {
            "provider-alias"
        }

        fn model_id(&self) -> &str {
            "provider-alias-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("provider-alias test uses chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let provider_name = request
                .tools
                .first()
                .expect("test request should advertise one tool")
                .name
                .clone();
            self.captured.lock().unwrap().push(request);
            let round = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let route = axocoatl_core::ProviderMetadata::from([
                ("axocoatl.route.slot".to_string(), "primary".to_string()),
                ("axocoatl.route.provider".to_string(), "gemini".to_string()),
                (
                    "axocoatl.route.model".to_string(),
                    "gemini-test".to_string(),
                ),
            ]);
            let events = if round == 0 {
                vec![
                    Ok(StreamEvent::ProviderRoute { metadata: route }),
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "provider-call-1".to_string(),
                        name: Some(provider_name),
                        args_delta: "{\"text\":\"hi\"}".to_string(),
                    }),
                    Ok(StreamEvent::ToolCallMetadata {
                        index: Some(0),
                        id: "provider-call-1".to_string(),
                        metadata: axocoatl_core::ProviderMetadata::from([(
                            "gemini.thought_signature".to_string(),
                            "opaque-signature".to_string(),
                        )]),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::ProviderRoute { metadata: route }),
                    Ok(StreamEvent::TextDelta {
                        delta: "alias round trip complete".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    /// Counts exactly the provider-visible tool-name bytes in declarations and
    /// replay history. This isolates the alias/preflight boundary from any
    /// provider tokenizer approximation.
    fn provider_wire_name_tokens(request: &ChatRequest) -> usize {
        let declaration_names = request
            .tools
            .iter()
            .map(|tool| tool.name.len())
            .sum::<usize>();
        let replay_names = request
            .messages
            .iter()
            .map(|message| {
                message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.len())
                    .sum::<usize>()
                    .saturating_add(message.name.as_ref().map_or(0, String::len))
            })
            .sum::<usize>();
        1_usize
            .saturating_add(declaration_names)
            .saturating_add(replay_names)
    }

    struct WireNameCostLlm {
        calls: std::sync::atomic::AtomicUsize,
        max_context_tokens: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        tool_first: bool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for WireNameCostLlm {
        fn provider_id(&self) -> &str {
            "wire-name-cost"
        }

        fn model_id(&self) -> &str {
            "wire-name-cost-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                max_context_tokens: self
                    .max_context_tokens
                    .load(std::sync::atomic::Ordering::SeqCst),
                max_output_tokens: 1,
                ..Default::default()
            }
        }

        fn count_tokens(&self, request: &ChatRequest) -> usize {
            provider_wire_name_tokens(request)
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("wire-name tests use chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let advertised = request.tools.first().map(|tool| tool.name.clone());
            self.captured.lock().unwrap().push(request);
            let round = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if self.tool_first && round == 0 {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "wire-call".to_string(),
                        name: advertised,
                        args_delta: r#"{"text":"hi"}"#.to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "wire-ok".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct CaptureToolNameHook(Arc<std::sync::Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl axocoatl_tools::ToolHook for CaptureToolNameHook {
        fn name(&self) -> &str {
            "capture_tool_name"
        }

        fn phases(&self) -> Vec<axocoatl_tools::HookPhase> {
            vec![axocoatl_tools::HookPhase::Pre]
        }

        async fn execute(
            &self,
            context: &axocoatl_tools::HookContext,
        ) -> axocoatl_tools::HookAction {
            self.0.lock().unwrap().push(context.tool_name.clone());
            axocoatl_tools::HookAction::Allow
        }
    }

    enum ProviderEvidenceScenario {
        MixedPolicy,
        Parallel { count: usize, content_bytes: usize },
    }

    struct ProviderEvidenceLlm {
        round: std::sync::atomic::AtomicUsize,
        scenario: ProviderEvidenceScenario,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ProviderEvidenceLlm {
        fn provider_id(&self) -> &str {
            "provider-evidence"
        }

        fn model_id(&self) -> &str {
            "provider-evidence-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        fn count_tokens(&self, _: &ChatRequest) -> usize {
            // Evidence-shape tests intentionally use a 256 KiB assistant
            // prelude. Tokenizer performance is unrelated to this seam.
            1
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("provider-evidence tests use chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if round > 0 {
                return Ok(Box::pin(tokio_stream::iter(vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "done".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ])));
            }

            let mut events = Vec::new();
            match &self.scenario {
                ProviderEvidenceScenario::MixedPolicy => {
                    let names = ["allow_tool", "deny_tool", "transform_tool"];
                    assert!(names
                        .iter()
                        .all(|name| request.tools.iter().any(|tool| tool.name == *name)));
                    events.push(Ok(StreamEvent::TextDelta {
                        delta: "assistant prelude".to_string(),
                    }));
                    for (index, name) in names.into_iter().enumerate() {
                        events.push(Ok(StreamEvent::ToolCallDelta {
                            index: Some(index),
                            id: format!("call-{index}"),
                            name: Some(name.to_string()),
                            args_delta: serde_json::json!({
                                "text": format!("original-{index}")
                            })
                            .to_string(),
                        }));
                    }
                }
                ProviderEvidenceScenario::Parallel {
                    count,
                    content_bytes,
                } => {
                    assert!(request.tools.iter().any(|tool| tool.name == "echo"));
                    events.push(Ok(StreamEvent::TextDelta {
                        delta: "x".repeat(*content_bytes),
                    }));
                    for index in 0..*count {
                        events.push(Ok(StreamEvent::ToolCallDelta {
                            index: Some(index),
                            id: format!("call-{index}"),
                            name: Some("echo".to_string()),
                            args_delta: serde_json::json!({"text": index.to_string()}).to_string(),
                        }));
                    }
                }
            }
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }));
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct MixedPolicyHook;

    #[async_trait::async_trait]
    impl axocoatl_tools::ToolHook for MixedPolicyHook {
        fn name(&self) -> &str {
            "mixed_policy"
        }

        fn phases(&self) -> Vec<axocoatl_tools::HookPhase> {
            vec![axocoatl_tools::HookPhase::Pre]
        }

        async fn execute(
            &self,
            context: &axocoatl_tools::HookContext,
        ) -> axocoatl_tools::HookAction {
            match context.tool_name.as_str() {
                "deny_tool" => axocoatl_tools::HookAction::Deny {
                    reason: "denied by test policy".to_string(),
                },
                "transform_tool" => axocoatl_tools::HookAction::Transform {
                    value: serde_json::json!({"text": "transformed"}),
                },
                _ => axocoatl_tools::HookAction::Allow,
            }
        }
    }

    struct PanickingActorHook {
        phase: axocoatl_tools::HookPhase,
    }

    #[async_trait::async_trait]
    impl axocoatl_tools::ToolHook for PanickingActorHook {
        fn name(&self) -> &str {
            "panicking_actor_hook"
        }

        fn phases(&self) -> Vec<axocoatl_tools::HookPhase> {
            vec![self.phase]
        }

        async fn execute(
            &self,
            _context: &axocoatl_tools::HookContext,
        ) -> axocoatl_tools::HookAction {
            panic!("intentional actor hook panic")
        }
    }

    struct IdlessSameNameThenTextLlm {
        round: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for IdlessSameNameThenTextLlm {
        fn provider_id(&self) -> &str {
            "gemini"
        }

        fn model_id(&self) -> &str {
            "gemini-idless-test"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("id-less ordering seam uses chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.captured.lock().unwrap().push(request);
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if round > 0 {
                return Ok(Box::pin(tokio_stream::iter(vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "ordered".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ])));
            }

            let mut events = Vec::new();
            for (index, text) in ["A", "B", "C"].into_iter().enumerate() {
                events.push(Ok(StreamEvent::ToolCallDelta {
                    index: Some(index),
                    id: String::new(),
                    name: Some("echo".to_string()),
                    args_delta: serde_json::json!({"text": text}).to_string(),
                }));
                events.push(Ok(StreamEvent::ToolCallMetadata {
                    index: Some(index),
                    id: String::new(),
                    metadata: axocoatl_core::ProviderMetadata::from([(
                        "gemini.thought_signature".to_string(),
                        format!("signature-{text}"),
                    )]),
                }));
            }
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }));
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct DenyMiddleEchoHook;

    #[async_trait::async_trait]
    impl axocoatl_tools::ToolHook for DenyMiddleEchoHook {
        fn name(&self) -> &str {
            "deny_middle_echo"
        }

        fn phases(&self) -> Vec<axocoatl_tools::HookPhase> {
            vec![axocoatl_tools::HookPhase::Pre]
        }

        async fn execute(
            &self,
            context: &axocoatl_tools::HookContext,
        ) -> axocoatl_tools::HookAction {
            if context
                .value
                .get("text")
                .and_then(serde_json::Value::as_str)
                == Some("B")
            {
                axocoatl_tools::HookAction::Deny {
                    reason: "middle denied".to_string(),
                }
            } else {
                axocoatl_tools::HookAction::Allow
            }
        }
    }

    #[derive(Clone, Copy)]
    enum InvalidToolStream {
        MalformedArguments,
        EmptyArguments,
        NonObjectArguments,
        EmptyName,
        UndeclaredCanonicalName,
        ConflictingId,
        ConflictingRoute,
        ToolUseWithoutCall,
        StopWithCall,
        HistoricalName,
        PrematureEof,
    }

    struct InvalidToolStreamLlm {
        mode: InvalidToolStream,
        canonical_name: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for InvalidToolStreamLlm {
        fn provider_id(&self) -> &str {
            "invalid-tool-stream"
        }

        fn model_id(&self) -> &str {
            "invalid-tool-stream-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("invalid stream tests use chat_stream")
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let advertised = request
                .tools
                .first()
                .map(|tool| tool.name.clone())
                .unwrap_or_default();
            let historical = request
                .messages
                .iter()
                .flat_map(|message| &message.tool_calls)
                .next()
                .map(|call| call.name.clone())
                .unwrap_or_default();
            let call = |id: &str, name: Option<String>, args_delta: &str| {
                Ok(StreamEvent::ToolCallDelta {
                    index: Some(0),
                    id: id.to_string(),
                    name,
                    args_delta: args_delta.to_string(),
                })
            };
            let mut events = match self.mode {
                InvalidToolStream::MalformedArguments => {
                    vec![call("bad-args", Some(advertised), "{\"text\":")]
                }
                InvalidToolStream::EmptyArguments => {
                    vec![call("empty-args", Some(advertised), "")]
                }
                InvalidToolStream::NonObjectArguments => {
                    vec![call("array-args", Some(advertised), "[]")]
                }
                InvalidToolStream::EmptyName => vec![call("empty-name", None, "{}")],
                InvalidToolStream::UndeclaredCanonicalName => vec![call(
                    "raw-canonical",
                    Some(self.canonical_name.clone()),
                    "{\"text\":\"must not run\"}",
                )],
                InvalidToolStream::ConflictingId => vec![
                    call("first-id", Some(advertised.clone()), "{"),
                    call("different-id", Some(advertised), "}"),
                ],
                InvalidToolStream::ConflictingRoute => vec![
                    Ok(StreamEvent::ProviderRoute {
                        metadata: axocoatl_core::ProviderMetadata::from([(
                            "axocoatl.route.slot".to_string(),
                            "primary".to_string(),
                        )]),
                    }),
                    Ok(StreamEvent::ProviderRoute {
                        metadata: axocoatl_core::ProviderMetadata::from([(
                            "axocoatl.route.slot".to_string(),
                            "fallback".to_string(),
                        )]),
                    }),
                ],
                InvalidToolStream::ToolUseWithoutCall => Vec::new(),
                InvalidToolStream::StopWithCall => {
                    vec![call("stop-with-call", Some(advertised), "{}")]
                }
                InvalidToolStream::HistoricalName => {
                    vec![call("historical", Some(historical), "{}")]
                }
                InvalidToolStream::PrematureEof => vec![Ok(StreamEvent::TextDelta {
                    delta: "partial response without Done".to_string(),
                })],
            };
            if !matches!(self.mode, InvalidToolStream::PrematureEof) {
                events.push(Ok(StreamEvent::Done {
                    finish_reason: if matches!(self.mode, InvalidToolStream::StopWithCall) {
                        FinishReason::Stop
                    } else {
                        FinishReason::ToolUse
                    },
                }));
            }
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct ExecutionCounterTool(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl axocoatl_tools::BuiltinTool for ExecutionCounterTool {
        fn description(&self) -> &str {
            "count executions"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({"executed": true}))
        }
    }

    struct PanickingActorTool;

    #[async_trait::async_trait]
    impl axocoatl_tools::BuiltinTool for PanickingActorTool {
        fn description(&self) -> &str {
            "panic for actor correlation testing"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
            panic!("intentional actor tool panic")
        }
    }

    /// Emits one tool call for `tool_rounds` responses, followed by a terminal
    /// response whose text may be empty. This models providers that keep
    /// retrying a failed edit instead of explaining what blocked them.
    struct ToolLoopLlm {
        calls: std::sync::atomic::AtomicUsize,
        tool_rounds: usize,
        tool_name: &'static str,
        final_content: &'static str,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolLoopLlm {
        fn provider_id(&self) -> &str {
            "tool-loop"
        }

        fn model_id(&self) -> &str {
            "tool-loop-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("tool-loop tests use chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let round = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if round < self.tool_rounds {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: format!("tool-loop-{round}"),
                        name: Some(self.tool_name.to_string()),
                        args_delta: "{\"text\":\"hi\"}".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                let mut events = Vec::new();
                if !self.final_content.is_empty() {
                    events.push(Ok(StreamEvent::TextDelta {
                        delta: self.final_content.to_string(),
                    }));
                }
                events.push(Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }));
                events
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct AlwaysFailTool;

    #[async_trait::async_trait]
    impl axocoatl_tools::BuiltinTool for AlwaysFailTool {
        fn description(&self) -> &str {
            "A test tool that always fails"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
            Err(axocoatl_tools::ToolError::ExecutionFailed {
                tool: "always_fail".to_string(),
                reason: "expected test failure".to_string(),
            })
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

    #[test]
    fn structured_compaction_archive_is_bounded_and_keeps_newest_contiguous_suffix() {
        let messages = (0..100)
            .map(|index| ChatMessage::assistant(format!("archive-{index}:{}", "x".repeat(70_000))))
            .collect::<Vec<_>>();
        let (archived, omitted) = structured_compaction_archive(&messages);
        assert!(omitted > 0);
        assert_eq!(archived.len() + omitted, messages.len());
        assert!(archived
            .last()
            .and_then(|message| message["content"]["Text"].as_str())
            .is_some_and(|content| content.starts_with("archive-99:")));
        let envelope = serde_json::json!({
            "reason": "context_compaction",
            "target_threshold": 100,
            "messages": archived,
            "archive_truncated": true,
            "omitted_messages": omitted,
            "original_message_count": messages.len(),
        });
        assert!(serde_json::to_vec(&envelope).unwrap().len() <= COMPACTION_ARCHIVE_MAX_BYTES);
    }

    struct NeverCompletesLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for NeverCompletesLlm {
        fn provider_id(&self) -> &str {
            "never"
        }

        fn model_id(&self) -> &str {
            "never-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("controlled execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(tokio_stream::pending()))
        }
    }

    struct PartialThenPendingLlm;

    #[async_trait::async_trait]
    impl LlmProvider for PartialThenPendingLlm {
        fn provider_id(&self) -> &str {
            "partial"
        }

        fn model_id(&self) -> &str {
            "partial-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("controlled execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            use tokio_stream::StreamExt;
            let emitted = tokio_stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    delta: "partial answer".to_string(),
                }),
                Ok(StreamEvent::Usage(TokenUsageStats::new(7, 2))),
            ]);
            Ok(Box::pin(emitted.chain(tokio_stream::pending())))
        }
    }

    struct PartialNoUsageThenPendingLlm;

    #[async_trait::async_trait]
    impl LlmProvider for PartialNoUsageThenPendingLlm {
        fn provider_id(&self) -> &str {
            "partial-no-usage"
        }

        fn model_id(&self) -> &str {
            "partial-no-usage-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("controlled execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            use tokio_stream::StreamExt;
            let emitted = tokio_stream::iter(vec![Ok(StreamEvent::TextDelta {
                delta: "partial without usage".to_string(),
            })]);
            Ok(Box::pin(emitted.chain(tokio_stream::pending())))
        }
    }

    struct ToolThenPartialNoUsageLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolThenPartialNoUsageLlm {
        fn provider_id(&self) -> &str {
            "tool-then-partial-no-usage"
        }

        fn model_id(&self) -> &str {
            "tool-then-partial-no-usage-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("controlled execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            use tokio_stream::StreamExt;
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                return Ok(Box::pin(tokio_stream::iter(vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "echo-before-partial".to_string(),
                        name: Some("echo".to_string()),
                        args_delta: "{\"text\":\"ok\"}".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(2, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ])));
            }
            let emitted = tokio_stream::iter(vec![Ok(StreamEvent::TextDelta {
                delta: "partial followup without usage".to_string(),
            })]);
            Ok(Box::pin(emitted.chain(tokio_stream::pending())))
        }
    }

    struct ToolThenTransportFailureLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolThenTransportFailureLlm {
        fn provider_id(&self) -> &str {
            "tool-then-transport-failure"
        }

        fn model_id(&self) -> &str {
            "tool-then-transport-failure-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("stateful execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                return Ok(Box::pin(tokio_stream::iter(vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "echo-before-transport-failure".to_string(),
                        name: Some("echo".to_string()),
                        args_delta: "{\"text\":\"ok\"}".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(80, 20))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ])));
            }
            Err(ProviderError::ApiError {
                provider: self.provider_id().to_string(),
                status: 503,
                message: "followup transport failed without usage".to_string(),
            })
        }
    }

    struct PartialToolThenPendingLlm {
        emitted: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for PartialToolThenPendingLlm {
        fn provider_id(&self) -> &str {
            "partial-tool"
        }

        fn model_id(&self) -> &str {
            "partial-tool-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("controlled execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let emitted = self.emitted.clone();
            Ok(Box::pin(async_stream::stream! {
                yield Ok(StreamEvent::ToolCallDelta {
                    index: Some(0),
                    id: "partial-call".to_string(),
                    name: Some("partial_tool".to_string()),
                    args_delta: "{\"unfinished\":".to_string(),
                });
                emitted.notify_one();
                std::future::pending::<()>().await;
            }))
        }
    }

    struct OneToolLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for OneToolLlm {
        fn provider_id(&self) -> &str {
            "one-tool"
        }

        fn model_id(&self) -> &str {
            "one-tool-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("controlled execution uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if call == 0 {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "side-effect-1".to_string(),
                        name: Some("side_effect".to_string()),
                        args_delta: "{}".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "clean next turn".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct GatedSideEffectTool {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl axocoatl_tools::BuiltinTool for GatedSideEffectTool {
        fn description(&self) -> &str {
            "A side-effecting test tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
            self.started.notify_one();
            self.release.notified().await;
            self.completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({"changed": true}))
        }
    }

    #[tokio::test]
    async fn controlled_execution_cancelled_before_start_skips_provider() {
        use crate::actor_impl::{execute_agent_streaming_controlled_measured, AgentActor};
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};
        use ractor::Actor;

        let provider = Arc::new(NeverCompletesLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
        let (actor, handle) = AgentActor::spawn(
            Some("cancel-before-provider".to_string()),
            AgentActor,
            (AgentConfig::default(), Box::new(behavior)),
        )
        .await
        .unwrap();
        let (sink, _chunks) = tokio::sync::mpsc::unbounded_channel();
        let control = AgentRunControl::new(AgentRunId::new("turn-before"));
        control.cancel();

        let measured = execute_agent_streaming_controlled_measured(
            &actor,
            AgentInput::text("do not send"),
            sink,
            control,
        )
        .await
        .unwrap();
        assert_eq!(
            measured.token_usage,
            axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::default())
        );
        match measured.outcome {
            AgentRunOutcome::Cancelled {
                run_id,
                partial_output,
            } => {
                assert_eq!(run_id.as_str(), "turn-before");
                assert!(partial_output.content.is_empty());
                assert_eq!(partial_output.token_usage.total(), 0);
            }
            AgentRunOutcome::Completed(_) => panic!("cancelled run reported complete"),
        }
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn controlled_execution_mid_stream_returns_partial_content_and_usage() {
        use crate::actor_impl::{
            execute_agent_streaming_controlled_measured, get_agent_measured_token_usage, AgentActor,
        };
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};
        use axocoatl_memory::{CheckpointPolicy, CheckpointStore, SemanticMemory};
        use ractor::Actor;

        let data = tempfile::tempdir().unwrap();
        let checkpoint_store = Arc::new(CheckpointStore::new(
            data.path().join("checkpoints"),
            CheckpointPolicy::EveryLlmCall,
        ));
        let semantic = Arc::new(
            SemanticMemory::new_hashed("cancel-mid-stream", data.path().join("semantic")).unwrap(),
        );
        let behavior = DefaultAgentBehavior::new(Arc::new(PartialThenPendingLlm), simple_counter())
            .with_checkpoint_store(checkpoint_store.clone())
            .with_semantic_memory(semantic.clone());
        let config = AgentConfig {
            id: AgentId::new("cancel-mid-stream"),
            name: "Cancel Mid Stream".to_string(),
            ..AgentConfig::default()
        };
        let (actor, handle) = AgentActor::spawn(
            Some("cancel-mid-stream".to_string()),
            AgentActor,
            (config, Box::new(behavior)),
        )
        .await
        .unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        let control = AgentRunControl::new(AgentRunId::new("turn-stream"));
        let caller_control = control.clone();
        let actor_for_turn = actor.clone();
        let turn = tokio::spawn(async move {
            execute_agent_streaming_controlled_measured(
                &actor_for_turn,
                AgentInput::text("stream"),
                sink,
                control,
            )
            .await
        });

        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), chunks.recv())
            .await
            .expect("provider did not stream")
            .expect("stream sink closed");
        assert!(matches!(
            chunk,
            crate::behavior::AgentStreamChunk::Text(ref text) if text == "partial answer"
        ));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        caller_control.cancel();

        let measured = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("controlled stream did not stop")
            .unwrap()
            .unwrap();
        assert_eq!(
            measured.token_usage,
            axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::new(7, 2)),
            "a received Usage frame remains exact even though cancellation wins before Done"
        );
        match measured.outcome {
            AgentRunOutcome::Cancelled { partial_output, .. } => {
                assert_eq!(partial_output.content, "partial answer");
                assert_eq!(partial_output.token_usage.input_tokens, 7);
                assert_eq!(partial_output.token_usage.output_tokens, 2);
            }
            AgentRunOutcome::Completed(_) => panic!("cancelled stream reported complete"),
        }
        let checkpoint = checkpoint_store
            .load_latest(&AgentId::new("cancel-mid-stream"))
            .await
            .unwrap()
            .expect("cancelled actor-session turn should checkpoint its partial transcript");
        assert_eq!(checkpoint.session_messages.len(), 2);
        assert_eq!(checkpoint.session_messages[0].content, "stream");
        assert_eq!(checkpoint.session_messages[1].content, "partial answer");
        assert!(
            semantic.is_empty(),
            "a partial cancelled exchange must not become a durable semantic fact"
        );
        let cumulative = get_agent_measured_token_usage(&actor).await.unwrap();
        assert!(cumulative.complete);
        assert_eq!(cumulative.usage, TokenUsageStats::new(7, 2));

        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_partial_without_usage_is_incomplete_for_stateful_and_stateless_calls() {
        use crate::actor_impl::{
            execute_agent_streaming_controlled_measured, get_agent_measured_token_usage, AgentActor,
        };
        use crate::behavior::AgentStreamChunk;
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};
        use axocoatl_memory::{CheckpointPolicy, CheckpointStore};
        use ractor::Actor;

        for stateless in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let checkpoints = Arc::new(CheckpointStore::new(data.path(), CheckpointPolicy::Manual));
            let id = AgentId::new(if stateless {
                "cancel-no-usage-stateless"
            } else {
                "cancel-no-usage-stateful"
            });
            let behavior =
                DefaultAgentBehavior::new(Arc::new(PartialNoUsageThenPendingLlm), simple_counter())
                    .with_checkpoint_store(checkpoints.clone());
            let config = AgentConfig {
                id: id.clone(),
                ..AgentConfig::default()
            };
            let (actor, handle) = AgentActor::spawn(
                Some(format!("{id}-actor")),
                AgentActor,
                (config, Box::new(behavior)),
            )
            .await
            .unwrap();
            let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
            let control = AgentRunControl::new(AgentRunId::new(format!("{id}-run")));
            let caller_control = control.clone();
            let actor_for_turn = actor.clone();
            let mut input = AgentInput::text("partial request");
            if stateless {
                input = input.with_stateless(true);
            }
            let turn = tokio::spawn(async move {
                execute_agent_streaming_controlled_measured(&actor_for_turn, input, sink, control)
                    .await
            });

            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), chunks.recv())
                .await
                .expect("provider did not stream partial content")
                .expect("stream sink closed");
            assert!(matches!(
                chunk,
                AgentStreamChunk::Text(ref text) if text == "partial without usage"
            ));
            caller_control.cancel();
            let measured = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
                .await
                .expect("controlled stream did not stop")
                .unwrap()
                .unwrap();
            let partial = match measured.outcome {
                AgentRunOutcome::Cancelled { partial_output, .. } => partial_output,
                AgentRunOutcome::Completed(_) => panic!("cancelled stream reported complete"),
            };
            assert_eq!(partial.content, "partial without usage");
            assert!(partial.token_usage.total() > 0);
            assert_eq!(measured.token_usage.usage, partial.token_usage);
            assert!(
                !measured.token_usage.complete,
                "a cancelled nonterminal stream without Usage is only a numeric lower bound"
            );
            let cumulative = get_agent_measured_token_usage(&actor).await.unwrap();
            assert!(!cumulative.complete);
            assert_eq!(cumulative.usage, partial.token_usage);
            let checkpoint = checkpoints.load_latest(&id).await.unwrap().unwrap();
            assert!(!checkpoint.cumulative_token_usage_known);
            assert_eq!(checkpoint.cumulative_token_usage, partial.token_usage);
            if stateless {
                assert!(checkpoint.session_messages.is_empty());
            } else {
                assert_eq!(checkpoint.session_messages.len(), 2);
            }

            actor.stop(None);
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_tool_followup_without_usage_remains_incomplete() {
        use crate::actor_impl::{
            execute_agent_streaming_controlled_measured, get_agent_measured_token_usage, AgentActor,
        };
        use crate::behavior::AgentStreamChunk;
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};
        use axocoatl_tools::{EchoTool, ToolExecutor};
        use ractor::Actor;

        let provider = Arc::new(ToolThenPartialNoUsageLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        let (actor, handle) = AgentActor::spawn(
            Some("cancel-followup-no-usage".to_string()),
            AgentActor,
            (AgentConfig::default(), Box::new(behavior)),
        )
        .await
        .unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        let control = AgentRunControl::new(AgentRunId::new("followup-no-usage"));
        let caller_control = control.clone();
        let actor_for_turn = actor.clone();
        let turn = tokio::spawn(async move {
            execute_agent_streaming_controlled_measured(
                &actor_for_turn,
                AgentInput::text("call echo then continue"),
                sink,
                control,
            )
            .await
        });

        loop {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), chunks.recv())
                .await
                .expect("provider did not reach partial followup")
                .expect("stream sink closed");
            if matches!(
                chunk,
                AgentStreamChunk::Text(ref text) if text == "partial followup without usage"
            ) {
                break;
            }
        }
        caller_control.cancel();
        let measured = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("controlled followup did not stop")
            .unwrap()
            .unwrap();
        let partial = match measured.outcome {
            AgentRunOutcome::Cancelled { partial_output, .. } => partial_output,
            AgentRunOutcome::Completed(_) => panic!("cancelled followup reported complete"),
        };
        assert_eq!(partial.tool_calls.len(), 1);
        assert_eq!(partial.content, "partial followup without usage");
        assert!(partial.token_usage.total() > TokenUsageStats::new(2, 1).total());
        assert_eq!(measured.token_usage.usage, partial.token_usage);
        assert!(!measured.token_usage.complete);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let cumulative = get_agent_measured_token_usage(&actor).await.unwrap();
        assert!(!cumulative.complete);
        assert_eq!(cumulative.usage, partial.token_usage);

        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn failed_followup_preserves_reported_subtotal_as_incomplete_across_restart() {
        use crate::actor_impl::{
            execute_agent_measured, get_agent_measured_token_usage, AgentActor,
        };
        use axocoatl_memory::{CheckpointPolicy, CheckpointStore};
        use axocoatl_tools::{EchoTool, ToolExecutor};
        use ractor::Actor;

        let data = tempfile::tempdir().unwrap();
        let checkpoints = Arc::new(CheckpointStore::new(
            data.path().join("checkpoints"),
            CheckpointPolicy::Manual,
        ));
        let provider = Arc::new(ToolThenTransportFailureLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_checkpoint_store(checkpoints.clone());
        let config = AgentConfig {
            id: AgentId::new("failed-followup-subtotal"),
            ..AgentConfig::default()
        };
        let (actor, handle) = AgentActor::spawn(
            Some("failed-followup-subtotal-first".to_string()),
            AgentActor,
            (config.clone(), Box::new(behavior)),
        )
        .await
        .unwrap();

        let failure = execute_agent_measured(&actor, AgentInput::text("call echo, then answer"))
            .await
            .unwrap_err();
        assert!(failure.message.contains("followup transport failed"));
        assert_eq!(failure.token_usage.usage, TokenUsageStats::new(80, 20));
        assert!(
            !failure.token_usage.complete,
            "the reported first-call subtotal remains visible while the failed followup is unknown"
        );
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        actor.kill();
        handle.await.unwrap();

        let checkpoint = checkpoints.load_latest(&config.id).await.unwrap().unwrap();
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(80, 20)
        );
        assert!(!checkpoint.cumulative_token_usage_known);
        assert!(
            checkpoint.session_messages.is_empty(),
            "a failed active turn must not persist an orphan user/tool transaction"
        );

        let restored_behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("restored", 1, 1)), simple_counter())
                .with_checkpoint_store(checkpoints);
        let (restored_actor, restored_handle) = AgentActor::spawn(
            Some("failed-followup-subtotal-restored".to_string()),
            AgentActor,
            (config, Box::new(restored_behavior)),
        )
        .await
        .unwrap();
        let restored = get_agent_measured_token_usage(&restored_actor)
            .await
            .unwrap();
        assert_eq!(restored.usage, TokenUsageStats::new(80, 20));
        assert!(!restored.complete);
        restored_actor.stop(None);
        restored_handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_discards_partial_tool_arguments_without_provider_error() {
        use crate::actor_impl::{execute_agent_streaming_controlled, AgentActor};
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};
        use ractor::Actor;

        let emitted = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(PartialToolThenPendingLlm {
            emitted: emitted.clone(),
        });
        let behavior = DefaultAgentBehavior::new(provider, simple_counter());
        let (actor, handle) = AgentActor::spawn(
            Some("cancel-partial-tool".to_string()),
            AgentActor,
            (AgentConfig::default(), Box::new(behavior)),
        )
        .await
        .unwrap();
        let (sink, _chunks) = tokio::sync::mpsc::unbounded_channel();
        let control = AgentRunControl::new(AgentRunId::new("turn-partial-tool"));
        let caller_control = control.clone();
        let actor_for_turn = actor.clone();
        let turn = tokio::spawn(async move {
            execute_agent_streaming_controlled(
                &actor_for_turn,
                AgentInput::text("start partial tool"),
                sink,
                control,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), emitted.notified())
            .await
            .expect("provider did not emit partial tool arguments");
        caller_control.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("controlled stream did not stop")
            .unwrap()
            .expect("cancellation must not become a provider error");
        assert!(matches!(
            outcome,
            AgentRunOutcome::Cancelled { partial_output, .. }
                if partial_output.tool_calls.is_empty()
        ));

        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_waits_for_started_side_effecting_tool_boundary() {
        use crate::actor_impl::{execute_agent_streaming_controlled, AgentActor};
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};
        use axocoatl_tools::ToolExecutor;
        use ractor::Actor;

        let provider = Arc::new(OneToolLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut executor = ToolExecutor::new();
        executor.register_builtin(
            "side_effect",
            Arc::new(GatedSideEffectTool {
                started: started.clone(),
                release: release.clone(),
                completed: completed.clone(),
            }),
        );
        let behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        let (actor, handle) = AgentActor::spawn(
            Some("cancel-during-tool".to_string()),
            AgentActor,
            (AgentConfig::default(), Box::new(behavior)),
        )
        .await
        .unwrap();
        let (sink, _chunks) = tokio::sync::mpsc::unbounded_channel();
        let control = AgentRunControl::new(AgentRunId::new("turn-tool"));
        let caller_control = control.clone();
        let actor_for_turn = actor.clone();
        let turn = tokio::spawn(async move {
            execute_agent_streaming_controlled(
                &actor_for_turn,
                AgentInput::text("change something"),
                sink,
                control,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("tool did not start");
        caller_control.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!turn.is_finished(), "started tool future was dropped");
        assert!(!completed.load(std::sync::atomic::Ordering::SeqCst));

        release.notify_one();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("turn did not finish after tool reached its boundary")
            .unwrap()
            .unwrap();
        match outcome {
            AgentRunOutcome::Cancelled { partial_output, .. } => {
                assert_eq!(partial_output.tool_calls.len(), 1);
                assert_eq!(partial_output.tool_calls[0].tool_name, "side_effect");
                assert_eq!(
                    partial_output.tool_calls[0].result,
                    Some(serde_json::json!({"changed": true}))
                );
            }
            AgentRunOutcome::Completed(_) => panic!("cancelled tool turn reported complete"),
        }
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation must stop before the follow-up provider call"
        );

        let (next_sink, _next_chunks) = tokio::sync::mpsc::unbounded_channel();
        let next_outcome = execute_agent_streaming_controlled(
            &actor,
            AgentInput::text("continue after the stopped turn"),
            next_sink,
            AgentRunControl::new(AgentRunId::new("turn-after-stop")),
        )
        .await
        .unwrap();
        match next_outcome {
            AgentRunOutcome::Completed(output) => {
                assert_eq!(output.content, "clean next turn");
            }
            AgentRunOutcome::Cancelled { .. } => {
                panic!("cancellation ownership leaked into the next turn")
            }
        }
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the next turn must get a fresh provider call after Stop"
        );

        actor.stop(None);
        handle.await.unwrap();
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

    struct BudgetProbeLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        reported_usage: Option<TokenUsageStats>,
        tool_first: bool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for BudgetProbeLlm {
        fn provider_id(&self) -> &str {
            "budget-probe"
        }

        fn model_id(&self) -> &str {
            "budget-probe-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                max_context_tokens: 10_000,
                max_output_tokens: 1_000,
                ..Default::default()
            }
        }

        fn count_tokens(&self, _: &ChatRequest) -> usize {
            10
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.captured.lock().unwrap().push(request);
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ChatResponse {
                content: "summary".to_string(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: self.reported_usage.clone().unwrap_or_default(),
                model: self.model_id().to_string(),
                provider: self.provider_id().to_string(),
            })
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let advertised_tool = request.tools.first().map(|tool| tool.name.clone());
            self.captured.lock().unwrap().push(request);
            let round = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut events = if self.tool_first && round == 0 {
                vec![Ok(StreamEvent::ToolCallDelta {
                    index: Some(0),
                    id: "budget-tool-call".to_string(),
                    name: advertised_tool,
                    args_delta: serde_json::json!({"text": "x".repeat(260)}).to_string(),
                })]
            } else {
                vec![Ok(StreamEvent::TextDelta {
                    delta: "budget response".to_string(),
                })]
            };
            if let Some(usage) = &self.reported_usage {
                events.push(Ok(StreamEvent::Usage(usage.clone())));
            }
            events.push(Ok(StreamEvent::Done {
                finish_reason: if self.tool_first && round == 0 {
                    FinishReason::ToolUse
                } else {
                    FinishReason::Stop
                },
            }));
            Ok(Box::pin(tokio_stream::iter(events)))
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
        assert_eq!(output.token_usage.input_tokens, 28);
        assert_eq!(output.token_usage.output_tokens, 8);

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

    #[tokio::test]
    async fn parallel_tool_panic_preserves_provider_order_identity_and_followup() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ParallelPanicThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("panic_tool", Arc::new(PanickingActorTool));
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));

        let output = behavior
            .execute(AgentInput::text("run both parallel tools"))
            .await
            .unwrap();

        assert_eq!(output.content, "parallel panic handled");
        assert_eq!(output.tool_calls.len(), 2);
        assert_eq!(output.tool_calls[0].tool_name, "panic_tool");
        assert!(output.tool_calls[0]
            .result
            .as_ref()
            .is_some_and(|result| result.get("error").is_some()));
        assert_eq!(output.tool_calls[1].tool_name, "echo");
        assert_eq!(
            output.tool_calls[1]
                .result
                .as_ref()
                .and_then(|result| result.get("text"))
                .and_then(serde_json::Value::as_str),
            Some("ok")
        );

        let mut started = Vec::new();
        let mut finished = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            match chunk {
                AgentStreamChunk::ToolCallStarted { id, name, .. } => {
                    started.push((id, name));
                }
                AgentStreamChunk::ToolCallResult {
                    id, name, is_error, ..
                } => finished.push((id, name, is_error)),
                _ => {}
            }
        }
        assert_eq!(
            started,
            vec![
                ("call-a".to_string(), "panic_tool".to_string()),
                ("call-b".to_string(), "echo".to_string()),
            ]
        );
        assert_eq!(
            finished,
            vec![
                ("call-a".to_string(), "panic_tool".to_string(), true),
                ("call-b".to_string(), "echo".to_string(), false),
            ]
        );

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let followup = &requests[1];
        let assistant = followup
            .messages
            .iter()
            .find(|message| !message.tool_calls.is_empty())
            .expect("follow-up retains the parallel assistant call group");
        assert_eq!(
            assistant
                .tool_calls
                .iter()
                .map(|call| (call.id.as_str(), call.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("call-a", "panic_tool"), ("call-b", "echo")]
        );
        let results = followup
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id.as_deref(), Some("call-a"));
        assert_eq!(results[0].name.as_deref(), Some("panic_tool"));
        assert!(results[0]
            .text_content()
            .is_some_and(|content| content.contains("panicked")));
        assert_eq!(results[1].tool_call_id.as_deref(), Some("call-b"));
        assert_eq!(results[1].name.as_deref(), Some("echo"));
        assert!(results[1]
            .text_content()
            .is_some_and(|content| content.contains("\"text\":\"ok\"")));
    }

    #[tokio::test]
    async fn direct_text_recovery_seeds_provider_metadata_for_replay() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(TextToolThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let output = behavior
            .execute(AgentInput::text("echo through text recovery"))
            .await
            .unwrap();
        assert_eq!(output.content, "text recovery complete");

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let replayed_call = requests[1]
            .messages
            .iter()
            .find_map(|message| message.tool_calls.first())
            .expect("follow-up must replay the recovered assistant tool call");
        assert_eq!(replayed_call.name, "echo");
        assert_eq!(replayed_call.id.len(), 9);
        assert!(replayed_call
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric()));
        assert_eq!(
            replayed_call
                .provider_metadata
                .get(axocoatl_llm::TOOL_METADATA_PROVIDER_ID)
                .map(String::as_str),
            Some("ollama")
        );
        assert!(!replayed_call.provider_metadata.is_empty());
    }

    #[tokio::test]
    async fn native_replay_providers_do_not_dispatch_plain_text_json_tools() {
        use axocoatl_tools::ToolExecutor;

        for provider_id in ["mistral", "gemini", "anthropic"] {
            let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let provider = Arc::new(TextJsonOnlyLlm {
                provider: provider_id,
                content: r#"{"echo":{"text":"must stay text"}}"#.to_string(),
                reported_usage: None,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut executor = ToolExecutor::new();
            executor.register_builtin("echo", Arc::new(ExecutionCounterTool(executions.clone())));
            let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
                .with_tool_executor(Arc::new(executor));
            behavior.on_start(&AgentConfig::default()).await.unwrap();

            let output = behavior
                .execute(AgentInput::text("return a JSON-looking tool call"))
                .await
                .unwrap();

            assert_eq!(output.content, r#"{"echo":{"text":"must stay text"}}"#);
            assert!(output.tool_calls.is_empty());
            assert_eq!(
                executions.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{provider_id} text cannot be promoted without native replay evidence"
            );
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn fallback_selected_ollama_controls_text_recovery_and_provenance() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let primary = Arc::new(RateLimitedPrimaryLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ollama = Arc::new(TextToolThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
        });
        let provider = Arc::new(axocoatl_llm::FallbackProvider::new(
            primary.clone(),
            Some(axocoatl_llm::FallbackTarget {
                provider: ollama,
                model: "ollama-like-model".to_string(),
            }),
        ));
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let output = behavior
            .execute(AgentInput::text("recover on the selected local route"))
            .await
            .unwrap();

        assert_eq!(output.content, "text recovery complete");
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(
            primary.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the pinned follow-up must bypass the rate-limited primary"
        );
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let call = requests[1]
            .messages
            .iter()
            .find_map(|message| message.tool_calls.first())
            .expect("fallback follow-up must replay recovered call");
        assert_eq!(call.id.len(), 9);
        assert!(call.id.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        assert_eq!(
            call.provider_metadata
                .get(axocoatl_llm::TOOL_METADATA_PROVIDER_ID)
                .map(String::as_str),
            Some("ollama")
        );
        assert_eq!(
            call.provider_metadata
                .get(axocoatl_llm::TOOL_METADATA_ROUTE_PROVIDER)
                .map(String::as_str),
            Some("ollama")
        );
    }

    #[tokio::test]
    async fn excessive_ollama_text_recovery_fails_before_any_tool_dispatch() {
        use axocoatl_tools::ToolExecutor;

        let content = (0..=MAX_PROVIDER_TOOL_CALLS)
            .map(|_| r#"{"echo":{}}"#)
            .collect::<Vec<_>>()
            .join(" ");
        let provider = Arc::new(TextJsonOnlyLlm {
            provider: "ollama",
            content,
            reported_usage: Some(TokenUsageStats::new(17, 5).with_reasoning(3)),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(ExecutionCounterTool(executions.clone())));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("too many recovered calls"))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::Provider(message) if message.contains("128-call")));
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let usage = behavior.cumulative_token_usage_snapshot();
        assert_eq!(usage.input_tokens, 17);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.reasoning_tokens, Some(3));
    }

    #[tokio::test]
    async fn excessive_non_tool_json_candidates_fail_bounded_before_dispatch() {
        use axocoatl_tools::ToolExecutor;

        let content = (0..=MAX_TEXT_JSON_CANDIDATES)
            .map(|index| format!(r#"{{"not_a_tool_{index}":null}}"#))
            .collect::<Vec<_>>()
            .join(" ");
        let provider = Arc::new(TextJsonOnlyLlm {
            provider: "ollama",
            content,
            reported_usage: Some(TokenUsageStats::new(19, 7).with_reasoning(2)),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(ExecutionCounterTool(executions.clone())));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("many non-tool JSON candidates"))
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentError::Provider(message) if message.contains("256 top-level JSON candidates"))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let usage = behavior.cumulative_token_usage_snapshot();
        assert_eq!(usage.input_tokens, 19);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.reasoning_tokens, Some(2));
    }

    #[tokio::test]
    async fn failed_paid_recovery_restores_lifetime_usage_and_next_execute_gets_fresh_headroom() {
        use crate::actor_impl::{execute_agent, get_agent_token_usage, AgentActor};
        use axocoatl_memory::CheckpointPolicy;
        use axocoatl_tools::{EchoTool, ToolExecutor};
        use ractor::Actor;

        let data = tempfile::tempdir().unwrap();
        let store = Arc::new(CheckpointStore::new(
            data.path(),
            CheckpointPolicy::EveryLlmCall,
        ));
        let agent_id = AgentId::new("paid-recovery-restart");
        let mut config = test_config_with_budget(100);
        config.id = agent_id.clone();
        config.sampling.max_tokens = Some(20);

        let content = (0..=MAX_PROVIDER_TOOL_CALLS)
            .map(|_| r#"{"echo":{}}"#)
            .collect::<Vec<_>>()
            .join(" ");
        let failing_provider = Arc::new(TextJsonOnlyLlm {
            provider: "ollama",
            content,
            reported_usage: Some(TokenUsageStats::new(17, 5).with_reasoning(3)),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let behavior = DefaultAgentBehavior::new(failing_provider, simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_checkpoint_store(store.clone());
        let (actor, handle) = AgentActor::spawn(
            Some("paid-recovery-first".to_string()),
            AgentActor,
            (config.clone(), Box::new(behavior)),
        )
        .await
        .unwrap();

        let error = execute_agent(&actor, AgentInput::text("trigger recovery overflow"))
            .await
            .unwrap_err();
        assert!(error.contains("128-call"));
        let _ = handle.await;

        let checkpoint = store.load_latest(&agent_id).await.unwrap().unwrap();
        assert!(
            checkpoint.session_messages.is_empty(),
            "the failed paid turn must checkpoint only the last complete prefix"
        );
        assert_eq!(checkpoint.cumulative_token_usage.input_tokens, 17);
        assert_eq!(checkpoint.cumulative_token_usage.output_tokens, 5);
        assert_eq!(checkpoint.cumulative_token_usage.reasoning_tokens, Some(3));

        let blocked_provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: Some(TokenUsageStats::new(1, 1)),
            tool_first: false,
        });
        let mut blocked_config = config.clone();
        blocked_config.token_budget = Some(TokenBudget {
            per_call: 35,
            per_execution: 35,
            overflow_policy: OverflowPolicy::Abort,
        });
        let blocked_behavior =
            DefaultAgentBehavior::new(blocked_provider.clone(), simple_counter())
                .with_checkpoint_store(store.clone());
        let (blocked_actor, blocked_handle) = AgentActor::spawn(
            Some("paid-recovery-blocked".to_string()),
            AgentActor,
            (blocked_config, Box::new(blocked_behavior)),
        )
        .await
        .unwrap();
        let restored = get_agent_token_usage(&blocked_actor).await.unwrap();
        assert_eq!(restored.input_tokens, 17);
        assert_eq!(restored.output_tokens, 5);
        assert_eq!(restored.reasoning_tokens, Some(3));
        execute_agent(&blocked_actor, AgentInput::text("fresh activation"))
            .await
            .unwrap();
        assert_eq!(
            blocked_provider
                .calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let after_fresh_turn = get_agent_token_usage(&blocked_actor).await.unwrap();
        assert_eq!(after_fresh_turn.input_tokens, 18);
        assert_eq!(after_fresh_turn.output_tokens, 6);
        assert_eq!(after_fresh_turn.reasoning_tokens, Some(3));
        blocked_actor.stop(None);
        let _ = blocked_handle.await;

        let success_behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("ok", 2, 3)), simple_counter())
                .with_checkpoint_store(store.clone());
        let (success_actor, success_handle) = AgentActor::spawn(
            Some("paid-recovery-success".to_string()),
            AgentActor,
            (config, Box::new(success_behavior)),
        )
        .await
        .unwrap();
        let before = get_agent_token_usage(&success_actor).await.unwrap();
        assert_eq!(before.total(), 27);
        execute_agent(&success_actor, AgentInput::text("one successful call"))
            .await
            .unwrap();
        let after = get_agent_token_usage(&success_actor).await.unwrap();
        assert_eq!(after.input_tokens, 20);
        assert_eq!(after.output_tokens, 9);
        assert_eq!(after.reasoning_tokens, Some(3));
        assert_eq!(
            after.total(),
            32,
            "the successful call must merge exactly once"
        );

        success_actor.stop(None);
        success_handle.await.unwrap();
    }

    #[tokio::test]
    async fn structured_stream_rejects_129th_distinct_call_before_dispatch() {
        use axocoatl_tools::ToolExecutor;

        let provider = Arc::new(ManyStructuredCallsLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(ExecutionCounterTool(executions.clone())));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("stream too many structured calls"))
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentError::Provider(message) if message.contains("128 distinct tool calls"))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_alias_round_trip_keeps_canonical_name_for_hooks_dispatch_and_evidence() {
        use axocoatl_tools::{is_provider_tool_name, EchoTool, HookRegistry, ToolExecutor};

        let internal_name = format!(
            "mcp__{}__issues.list/🦀?state=open",
            "configured-server-".repeat(8)
        );
        let captured_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ProviderAliasThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured_requests.clone(),
        });
        let captured_hook_names = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut hooks = HookRegistry::new();
        hooks.register_global(Arc::new(CaptureToolNameHook(captured_hook_names.clone())));

        let mut executor = ToolExecutor::new();
        executor.register_builtin(internal_name.clone(), Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_hook_registry(Arc::new(hooks));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let output = behavior
            .execute(AgentInput::text("use the configured MCP-shaped tool"))
            .await
            .unwrap();
        assert_eq!(output.content, "alias round trip complete");
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].tool_name, internal_name);
        assert_eq!(
            output.tool_calls[0].result,
            Some(serde_json::json!({"text": "hi"}))
        );
        assert_eq!(
            captured_hook_names.lock().unwrap().as_slice(),
            std::slice::from_ref(&internal_name)
        );

        let requests = captured_requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "initial request plus tool-result follow-up"
        );
        let provider_name = &requests[0].tools[0].name;
        assert!(is_provider_tool_name(provider_name));
        assert_eq!(provider_name.len(), 64);
        assert_ne!(provider_name, &internal_name);
        assert_eq!(&requests[1].tools[0].name, provider_name);

        let assistant = requests[1]
            .messages
            .iter()
            .find(|message| !message.tool_calls.is_empty())
            .expect("follow-up should replay the provider-visible call");
        assert_eq!(&assistant.tool_calls[0].name, provider_name);
        let tool_result = requests[1]
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("follow-up should replay the provider-visible result");
        assert_eq!(tool_result.name.as_deref(), Some(provider_name.as_str()));
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("provider-call-1"));
        assert_eq!(
            assistant.tool_calls[0]
                .provider_metadata
                .get("axocoatl.route.provider")
                .map(String::as_str),
            Some("gemini")
        );
        assert_eq!(
            assistant.tool_calls[0]
                .provider_metadata
                .get("gemini.thought_signature")
                .map(String::as_str),
            Some("opaque-signature")
        );
    }

    #[tokio::test]
    async fn expanding_provider_alias_is_counted_before_context_dispatch() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let provider = Arc::new(WireNameCostLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            max_context_tokens: std::sync::atomic::AtomicUsize::new(10_000),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            tool_first: false,
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin(".", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior
            .on_start(&AgentConfig {
                sampling: axocoatl_core::SamplingConfig {
                    max_tokens: Some(1),
                    ..Default::default()
                },
                ..AgentConfig::default()
            })
            .await
            .unwrap();
        let input = AgentInput::text("context boundary");
        let canonical = behavior.build_request(&input);
        let capabilities = provider.capabilities();
        let canonical_required = behavior.request_context_tokens(&canonical, &capabilities);
        let (encoded, _) = DefaultAgentBehavior::encode_provider_request(canonical).unwrap();
        let encoded_required = behavior.request_context_tokens(&encoded, &capabilities);
        assert!(encoded_required > canonical_required);
        let limit = encoded_required - 1;
        assert!(canonical_required <= limit);
        provider
            .max_context_tokens
            .store(limit, std::sync::atomic::Ordering::SeqCst);

        let error = behavior.execute(input).await.unwrap_err();

        assert!(matches!(error, AgentError::ContextLimitExceeded { .. }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reserved_provider_alias_is_counted_before_abort_budget_dispatch() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let provider = Arc::new(WireNameCostLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            max_context_tokens: std::sync::atomic::AtomicUsize::new(10_000),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            tool_first: false,
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("axo_tool_x", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        let mut config = test_config_with_budget(20);
        config.sampling.max_tokens = Some(1);
        behavior.on_start(&config).await.unwrap();
        let input = AgentInput::text("budget boundary");
        let canonical = behavior.build_request(&input);
        let canonical_requested = provider.count_tokens(&canonical).saturating_add(1);
        let (encoded, _) = DefaultAgentBehavior::encode_provider_request(canonical).unwrap();
        let encoded_requested = provider.count_tokens(&encoded).saturating_add(1);
        assert!(canonical_requested <= 20);
        assert!(encoded_requested > 20);

        let error = behavior.execute(input).await.unwrap_err();

        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shortening_provider_alias_is_preflighted_in_wire_form_and_dispatches() {
        use axocoatl_tools::{is_provider_tool_name, EchoTool, ToolExecutor};

        let internal_name = format!("mcp__{}", "long-tool-name".repeat(20));
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(WireNameCostLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            max_context_tokens: std::sync::atomic::AtomicUsize::new(10_000),
            captured: captured.clone(),
            tool_first: false,
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin(internal_name.clone(), Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        let mut config = test_config_with_budget(80);
        config.sampling.max_tokens = Some(1);
        behavior.on_start(&config).await.unwrap();
        let input = AgentInput::text("shorten the provider name");
        let canonical = behavior.build_request(&input);
        assert!(provider.count_tokens(&canonical).saturating_add(1) > 80);

        let output = behavior.execute(input).await.unwrap();

        assert_eq!(output.content, "wire-ok");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let requests = captured.lock().unwrap();
        let provider_name = &requests[0].tools[0].name;
        assert!(is_provider_tool_name(provider_name));
        assert_eq!(provider_name.len(), 64);
        assert_ne!(provider_name, &internal_name);
        assert!(provider_wire_name_tokens(&requests[0]).saturating_add(1) <= 80);
    }

    #[tokio::test]
    async fn expanding_alias_in_followup_history_is_rejected_before_second_dispatch() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(WireNameCostLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            max_context_tokens: std::sync::atomic::AtomicUsize::new(10_000),
            captured: captured.clone(),
            tool_first: true,
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin(".", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        let mut config = test_config_with_budget(100);
        config.sampling.max_tokens = Some(1);
        behavior.on_start(&config).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("run the expanding tool"))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the encoded follow-up must fail before a second provider call"
        );
        assert_eq!(captured.lock().unwrap()[0].tools[0].name.len(), 64);

        let canonical_followup = behavior
            .build_request_from_session(None, None, 0, 0)
            .unwrap();
        assert!(provider.count_tokens(&canonical_followup).saturating_add(1) <= 100);
        let (encoded_followup, _) =
            DefaultAgentBehavior::encode_provider_request(canonical_followup).unwrap();
        assert!(provider.count_tokens(&encoded_followup).saturating_add(1) > 100);
        let replayed_names = encoded_followup
            .messages
            .iter()
            .flat_map(|message| {
                message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .chain(message.name.iter().map(String::as_str))
            })
            .collect::<Vec<_>>();
        assert_eq!(replayed_names.len(), 2);
        assert!(replayed_names.iter().all(|name| name.len() == 64));
    }

    #[tokio::test]
    async fn tool_start_evidence_preserves_provider_order_and_original_arguments_through_hooks() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::{EchoTool, HookRegistry, ToolExecutor};

        let provider = Arc::new(ProviderEvidenceLlm {
            round: std::sync::atomic::AtomicUsize::new(0),
            scenario: ProviderEvidenceScenario::MixedPolicy,
        });
        let mut executor = ToolExecutor::new();
        for name in ["allow_tool", "deny_tool", "transform_tool"] {
            executor.register_builtin(name, Arc::new(EchoTool));
        }
        let mut hooks = HookRegistry::new();
        hooks.register_global(Arc::new(MixedPolicyHook));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_hook_registry(Arc::new(hooks));
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));

        let output = behavior
            .execute(AgentInput::text("run all three"))
            .await
            .unwrap();
        assert_eq!(output.content, "done");
        let mut starts = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            if let AgentStreamChunk::ToolCallStarted {
                name,
                arguments,
                provider_arguments,
                assistant_content,
                provider_response_group,
                provider_call_index,
                provider_call_count,
                ..
            } = chunk
            {
                starts.push((
                    name,
                    arguments,
                    provider_arguments,
                    assistant_content,
                    provider_response_group,
                    provider_call_index,
                    provider_call_count,
                ));
            }
        }

        // All starts are deferred until pre-hooks finish, then surfaced once in
        // provider order. FIFO consumers therefore preserve A/B/C even when the
        // middle call is denied before parallel dispatch.
        assert_eq!(
            starts
                .iter()
                .map(|start| start.0.as_str())
                .collect::<Vec<_>>(),
            vec!["allow_tool", "deny_tool", "transform_tool"]
        );
        assert_eq!(
            starts.iter().map(|start| start.5).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(starts.iter().all(|start| start.4 == 1 && start.6 == 3));

        let allow = starts.iter().find(|start| start.0 == "allow_tool").unwrap();
        assert_eq!(allow.1, allow.2);
        assert_eq!(allow.3.as_deref(), Some("assistant prelude"));
        let denied = starts.iter().find(|start| start.0 == "deny_tool").unwrap();
        assert!(denied.3.is_none());
        let transformed = starts
            .iter()
            .find(|start| start.0 == "transform_tool")
            .unwrap();
        assert_eq!(transformed.1, serde_json::json!({"text": "transformed"}));
        assert_eq!(transformed.2, serde_json::json!({"text": "original-2"}));
        assert!(transformed.3.is_none());
    }

    #[tokio::test]
    async fn hook_panics_close_exact_tool_evidence_and_behavior_survives() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::{EchoTool, HookRegistry, ToolExecutor};

        for phase in [
            axocoatl_tools::HookPhase::Pre,
            axocoatl_tools::HookPhase::Post,
        ] {
            let provider = Arc::new(ProviderEvidenceLlm {
                round: std::sync::atomic::AtomicUsize::new(0),
                scenario: ProviderEvidenceScenario::MixedPolicy,
            });
            let mut executor = ToolExecutor::new();
            for name in ["allow_tool", "deny_tool", "transform_tool"] {
                executor.register_builtin(name, Arc::new(EchoTool));
            }
            let mut hooks = HookRegistry::new();
            hooks.register_global(Arc::new(PanickingActorHook { phase }));
            let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
                .with_tool_executor(Arc::new(executor))
                .with_hook_registry(Arc::new(hooks));
            behavior.on_start(&AgentConfig::default()).await.unwrap();
            let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
            behavior.set_stream_sink(Some(sink));

            let output = behavior
                .execute(AgentInput::text("exercise panic containment"))
                .await
                .unwrap();
            assert_eq!(output.content, "done");
            assert_eq!(output.tool_calls.len(), 3);
            assert!(output.tool_calls.iter().all(|call| {
                call.result
                    .as_ref()
                    .and_then(|result| result["error"].as_str())
                    .is_some_and(|error| error.contains("panicked"))
            }));

            let mut starts = Vec::new();
            let mut results = Vec::new();
            while let Ok(chunk) = chunks.try_recv() {
                match chunk {
                    AgentStreamChunk::ToolCallStarted {
                        id,
                        provider_call_index,
                        ..
                    } => starts.push((provider_call_index, id)),
                    AgentStreamChunk::ToolCallResult { id, result, .. } => {
                        results.push((id, result))
                    }
                    _ => {}
                }
            }
            assert_eq!(starts.len(), 3);
            assert_eq!(results.len(), 3);
            assert_eq!(
                starts.iter().map(|(_, id)| id).collect::<Vec<_>>(),
                results.iter().map(|(id, _)| id).collect::<Vec<_>>()
            );
            assert!(results.iter().all(|(_, result)| result["error"]
                .as_str()
                .is_some_and(|error| error.contains("panicked"))));

            let next = behavior
                .execute(AgentInput::text("the same behavior remains alive"))
                .await
                .unwrap();
            assert_eq!(next.content, "done");
        }
    }

    #[tokio::test]
    async fn idless_same_name_middle_denial_records_results_in_provider_order() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::{EchoTool, HookRegistry, ToolExecutor};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(IdlessSameNameThenTextLlm {
            round: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut hooks = HookRegistry::new();
        hooks.register_global(Arc::new(DenyMiddleEchoHook));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_hook_registry(Arc::new(hooks));
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));

        let output = behavior
            .execute(AgentInput::text("run the id-less parallel group"))
            .await
            .unwrap();
        assert_eq!(output.content, "ordered");
        assert_eq!(output.tool_calls.len(), 3);
        assert_eq!(
            output
                .tool_calls
                .iter()
                .map(|record| record.arguments["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
        assert_eq!(
            output.tool_calls[0].result,
            Some(serde_json::json!({"text": "A"}))
        );
        assert_eq!(
            output.tool_calls[1].result,
            Some(serde_json::json!({"error": "middle denied"}))
        );
        assert_eq!(
            output.tool_calls[2].result,
            Some(serde_json::json!({"text": "C"}))
        );

        let mut starts = Vec::new();
        let mut results = Vec::new();
        while let Ok(chunk) = chunks.try_recv() {
            match chunk {
                AgentStreamChunk::ToolCallStarted {
                    arguments,
                    provider_call_index,
                    ..
                } => starts.push((provider_call_index, arguments["text"].clone())),
                AgentStreamChunk::ToolCallResult { result, .. } => results.push(result),
                _ => {}
            }
        }
        assert_eq!(
            starts,
            vec![
                (0, serde_json::json!("A")),
                (1, serde_json::json!("B")),
                (2, serde_json::json!("C")),
            ]
        );
        assert_eq!(
            results,
            vec![
                serde_json::json!({"text": "A"}),
                serde_json::json!({"error": "middle denied"}),
                serde_json::json!({"text": "C"}),
            ]
        );

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let assistant_calls = requests[1]
            .messages
            .iter()
            .find(|message| !message.tool_calls.is_empty())
            .expect("follow-up replays the assistant group");
        assert_eq!(assistant_calls.tool_calls.len(), 3);
        assert!(assistant_calls
            .tool_calls
            .iter()
            .all(|call| call.id.is_empty() && call.name == "echo"));
        assert_eq!(
            assistant_calls
                .tool_calls
                .iter()
                .map(|call| {
                    call.provider_metadata
                        .get("gemini.thought_signature")
                        .map(String::as_str)
                        .unwrap()
                })
                .collect::<Vec<_>>(),
            vec!["signature-A", "signature-B", "signature-C"]
        );
        let replayed_results = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .map(|message| {
                serde_json::from_str::<serde_json::Value>(message.text_content().unwrap()).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            replayed_results,
            vec![
                serde_json::json!({"text": "A"}),
                serde_json::json!({"error": "middle denied"}),
                serde_json::json!({"text": "C"}),
            ]
        );
    }

    #[tokio::test]
    async fn parallel_tool_start_content_is_stored_once_at_the_max_call_shape() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::{EchoTool, ToolExecutor};

        const CALLS: usize = 128;
        const CONTENT_BYTES: usize = 256 * 1024;
        let provider = Arc::new(ProviderEvidenceLlm {
            round: std::sync::atomic::AtomicUsize::new(0),
            scenario: ProviderEvidenceScenario::Parallel {
                count: CALLS,
                content_bytes: CONTENT_BYTES,
            },
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            behavior.execute(AgentInput::text("run the parallel batch")),
        )
        .await
        .expect("parallel evidence batch timed out")
        .unwrap();
        let mut indexes = Vec::new();
        let mut retained_content_bytes = 0;
        let mut content_fields = 0;
        while let Ok(chunk) = receiver.try_recv() {
            if let AgentStreamChunk::ToolCallStarted {
                assistant_content,
                provider_call_index,
                provider_call_count,
                ..
            } = chunk
            {
                assert_eq!(provider_call_count, CALLS);
                indexes.push(provider_call_index);
                if let Some(content) = assistant_content {
                    content_fields += 1;
                    retained_content_bytes += content.len();
                    assert_eq!(provider_call_index, 0);
                }
            }
        }
        indexes.sort_unstable();
        assert_eq!(indexes, (0..CALLS).collect::<Vec<_>>());
        assert_eq!(content_fields, 1);
        assert_eq!(retained_content_bytes, CONTENT_BYTES);
    }

    #[tokio::test]
    async fn malformed_or_undeclared_streamed_tool_calls_fail_before_dispatch() {
        use axocoatl_tools::ToolExecutor;

        let canonical_name = format!(
            "mcp__{}__dangerous.default/action🦀",
            "configured-server-".repeat(8)
        );
        for mode in [
            InvalidToolStream::MalformedArguments,
            InvalidToolStream::EmptyArguments,
            InvalidToolStream::NonObjectArguments,
            InvalidToolStream::EmptyName,
            InvalidToolStream::UndeclaredCanonicalName,
            InvalidToolStream::ConflictingId,
            InvalidToolStream::ConflictingRoute,
            InvalidToolStream::ToolUseWithoutCall,
            InvalidToolStream::StopWithCall,
            InvalidToolStream::PrematureEof,
        ] {
            let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut executor = ToolExecutor::new();
            executor.register_builtin(
                canonical_name.clone(),
                Arc::new(ExecutionCounterTool(executions.clone())),
            );
            let provider = Arc::new(InvalidToolStreamLlm {
                mode,
                canonical_name: canonical_name.clone(),
            });
            let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
                .with_tool_executor(Arc::new(executor));
            behavior.on_start(&AgentConfig::default()).await.unwrap();

            let error = behavior
                .execute(AgentInput::text("run the dangerous tool"))
                .await
                .expect_err("invalid streamed call must fail closed");
            assert!(matches!(error, AgentError::Provider(_)), "{error:?}");
            assert_eq!(
                executions.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "no malformed, empty, non-object, unnamed, undeclared, or incoherent call may dispatch"
            );
        }
    }

    #[tokio::test]
    async fn history_only_provider_alias_is_not_callable_when_not_advertised() {
        use axocoatl_llm::{ConcurrencyPolicy, ToolDefinition};

        let historical = "mcp__historical server__old/tool🦀";
        let mut request = ChatRequest::simple("do not call history");
        request.tools.push(ToolDefinition {
            name: "echo".to_string(),
            description: "current tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: ConcurrencyPolicy::Safe,
        });
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "old-call".to_string(),
                    name: historical.to_string(),
                    arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                }],
            ));
        request.messages.push(ChatMessage::tool_result(
            "old result",
            historical,
            "old-call",
        ));

        let provider = Arc::new(InvalidToolStreamLlm {
            mode: InvalidToolStream::HistoricalName,
            canonical_name: historical.to_string(),
        });
        let behavior = DefaultAgentBehavior::new(provider, simple_counter());
        let (request, provider_tool_names) =
            DefaultAgentBehavior::encode_provider_request(request).unwrap();
        let error = match behavior.stream_chat(request, provider_tool_names).await {
            Err(error) => error,
            Ok(_) => panic!("history-only alias must not be callable"),
        };
        assert!(matches!(error, AgentError::Provider(_)), "{error:?}");
    }

    #[tokio::test]
    async fn terminal_tool_loop_rejects_pending_calls_after_safety_limit() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::ToolExecutor;

        let provider = Arc::new(ToolLoopLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            // Ten rounds execute; the eleventh provider response remains
            // pending and must never be mislabeled as a completed turn.
            tool_rounds: 11,
            tool_name: "always_fail",
            final_content: "",
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("always_fail", Arc::new(AlwaysFailTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));

        let error = behavior
            .execute(AgentInput::text("keep retrying the edit"))
            .await
            .unwrap_err();

        let AgentError::ToolFailed { tool, reason } = error else {
            panic!("unexpected terminal error: {error}");
        };
        assert_eq!(tool, "agent tool loop");
        assert!(reason.contains("safety limit of 10 rounds"));
        assert!(reason.contains("pending: always_fail"));
        assert!(reason.contains("pending calls were not executed"));
        assert!(reason.contains("Retry with a more capable model or narrow the task"));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 11);

        let mut evidence = Vec::new();
        while let Ok(chunk) = chunks.try_recv() {
            evidence.push(chunk);
        }
        assert_eq!(
            evidence
                .iter()
                .filter(|chunk| matches!(chunk, AgentStreamChunk::ToolCallStarted { .. }))
                .count(),
            10,
            "only calls that actually reached execution are surfaced as started"
        );
        assert_eq!(
            evidence
                .iter()
                .filter(|chunk| matches!(
                    chunk,
                    AgentStreamChunk::ToolCallResult { is_error: true, .. }
                ))
                .count(),
            10,
            "every executed failure remains available as stream evidence"
        );
    }

    #[tokio::test]
    async fn terminal_tool_loop_rejects_failed_or_undeclared_blank_completion() {
        use crate::behavior::AgentStreamChunk;
        use axocoatl_tools::ToolExecutor;

        let provider = Arc::new(ToolLoopLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool_rounds: 2,
            tool_name: "always_fail",
            final_content: "",
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("always_fail", Arc::new(AlwaysFailTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));

        let error = behavior
            .execute(AgentInput::text("make the edit"))
            .await
            .unwrap_err();
        let AgentError::ToolFailed { reason, .. } = error else {
            panic!("unexpected terminal error: {error}");
        };
        assert!(reason.contains("no final answer after 2 tool calls"));
        assert!(reason.contains("2 failed and 0 unresolved"));
        assert!(reason.contains("expected test failure"));
        assert!(reason.contains("Retry with a more capable model or narrow the task"));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let mut failure_results = 0;
        while let Ok(chunk) = chunks.try_recv() {
            if matches!(
                chunk,
                AgentStreamChunk::ToolCallResult { is_error: true, .. }
            ) {
                failure_results += 1;
            }
        }
        assert_eq!(failure_results, 2, "both failed attempts remain observable");

        let unresolved_provider = Arc::new(ToolLoopLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool_rounds: 1,
            tool_name: "unavailable_tool",
            final_content: "",
        });
        let mut unresolved = DefaultAgentBehavior::new(unresolved_provider, simple_counter());
        unresolved.on_start(&AgentConfig::default()).await.unwrap();
        let (sink, mut unresolved_chunks) = tokio::sync::mpsc::unbounded_channel();
        unresolved.set_stream_sink(Some(sink));
        let error = unresolved
            .execute(AgentInput::text("call a missing tool"))
            .await
            .unwrap_err();
        let AgentError::Provider(reason) = error else {
            panic!("unexpected undeclared-call error: {error}");
        };
        assert!(reason.contains("empty or undeclared tool-call name"));
        assert!(
            unresolved_chunks.try_recv().is_err(),
            "an undeclared call must fail before hooks, evidence, or dispatch"
        );
    }

    #[tokio::test]
    async fn terminal_tool_loop_preserves_valid_blank_and_explained_outcomes() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let successful_provider = Arc::new(ToolLoopLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool_rounds: 1,
            tool_name: "echo",
            final_content: "",
        });
        let mut successful_executor = ToolExecutor::new();
        successful_executor.register_builtin("echo", Arc::new(EchoTool));
        let mut successful = DefaultAgentBehavior::new(successful_provider, simple_counter())
            .with_tool_executor(Arc::new(successful_executor));
        successful.on_start(&AgentConfig::default()).await.unwrap();
        let output = successful
            .execute(AgentInput::text("perform a tool-only action"))
            .await
            .expect("successful tool-only completion remains valid");
        assert!(output.content.is_empty());
        assert_eq!(output.tool_calls.len(), 1);
        assert!(output.tool_calls[0].result.is_some());

        let mut initial_blank =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("", 0, 0)), simple_counter());
        initial_blank
            .on_start(&AgentConfig::default())
            .await
            .unwrap();
        let output = initial_blank
            .execute(AgentInput::text("an intentionally blank response"))
            .await
            .expect("a blank response without tool activity keeps its existing contract");
        assert!(output.content.is_empty());
        assert!(output.tool_calls.is_empty());

        let explained_provider = Arc::new(ToolLoopLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool_rounds: 1,
            tool_name: "always_fail",
            final_content: "I could not apply the edit because the expected text was absent.",
        });
        let mut explained_executor = ToolExecutor::new();
        explained_executor.register_builtin("always_fail", Arc::new(AlwaysFailTool));
        let mut explained = DefaultAgentBehavior::new(explained_provider, simple_counter())
            .with_tool_executor(Arc::new(explained_executor));
        explained.on_start(&AgentConfig::default()).await.unwrap();
        let output = explained
            .execute(AgentInput::text("attempt and explain"))
            .await
            .expect("a nonempty explanation after a failed tool remains valid");
        assert!(output.content.starts_with("I could not apply the edit"));
        assert_eq!(output.tool_calls.len(), 1);
        assert!(output.tool_calls[0]
            .result
            .as_ref()
            .is_some_and(|result| result.get("error").is_some()));
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
    async fn executor_tool_allowlist_inherits_filters_and_supports_exact_empty() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let make_executor = || {
            let mut executor = ToolExecutor::new();
            executor.register_builtin("echo", Arc::new(EchoTool));
            executor.register_builtin("always_fail", Arc::new(AlwaysFailTool));
            Arc::new(executor)
        };

        let mut inherited =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_tool_executor(make_executor());
        inherited.on_start(&AgentConfig::default()).await.unwrap();
        let mut inherited_names = inherited
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        inherited_names.sort();
        assert_eq!(inherited_names, vec!["always_fail", "echo"]);

        let mut filtered =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_tool_executor(make_executor());
        let filtered_config = AgentConfig {
            tools: vec!["echo".to_string()],
            ..AgentConfig::default()
        };
        filtered.on_start(&filtered_config).await.unwrap();
        let filtered_names = filtered
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(filtered_names, vec!["echo"]);

        let mut exact_empty =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_tool_executor(make_executor())
                .with_executor_tool_allowlist(Vec::new());
        exact_empty.on_start(&AgentConfig::default()).await.unwrap();
        assert!(exact_empty.tool_definitions().is_empty());
    }

    #[tokio::test]
    async fn unadvertised_allowlist_tool_never_reaches_hooks_or_dispatch() {
        use axocoatl_tools::{EchoTool, HookRegistry, ToolExecutor};

        let captured_hook_names = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut hooks = HookRegistry::new();
        hooks.register_global(Arc::new(CaptureToolNameHook(captured_hook_names.clone())));
        let provider = Arc::new(ToolCallsThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            first: vec![(
                "denied".to_string(),
                "always_fail".to_string(),
                "{}".to_string(),
            )],
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        executor.register_builtin("always_fail", Arc::new(AlwaysFailTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_hook_registry(Arc::new(hooks));
        let config = AgentConfig {
            tools: vec!["echo".to_string()],
            ..AgentConfig::default()
        };
        behavior.on_start(&config).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("try the disallowed tool"))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Provider(_)));
        assert!(captured_hook_names.lock().unwrap().is_empty());
        assert!(behavior.session().as_chat_messages().iter().all(|message| {
            message
                .tool_calls
                .iter()
                .all(|call| call.name != "always_fail")
        }));
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
    async fn consolidation_usage_is_visible_and_durable_across_actor_restart() {
        use crate::actor_impl::{consolidate_agent, get_agent_measured_token_usage, AgentActor};
        use axocoatl_memory::{CheckpointPolicy, CheckpointStore};
        use ractor::Actor;

        let dir = tempfile::tempdir().unwrap();
        let semantic = hashed_semantic(dir.path(), "durable fact to consolidate");
        let mut core = axocoatl_memory::CoreMemoryStore::new("a", dir.path().join("core.json"));
        core.ensure_block(axocoatl_memory::MemoryBlock::new("human", 0));
        let core = Arc::new(tokio::sync::RwLock::new(core));
        let checkpoints = Arc::new(CheckpointStore::new(
            dir.path().join("checkpoints"),
            CheckpointPolicy::Manual,
        ));
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: Some(TokenUsageStats::new(10, 4).with_reasoning(7)),
            tool_first: false,
        });
        let config = AgentConfig {
            id: AgentId::new("consolidation-usage"),
            ..AgentConfig::default()
        };
        let behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_semantic_memory(semantic.clone())
            .with_core_memory(core.clone(), std::collections::HashMap::new())
            .with_checkpoint_store(checkpoints.clone());
        let (actor, handle) = AgentActor::spawn(
            Some("consolidation-usage-first".to_string()),
            AgentActor,
            (config.clone(), Box::new(behavior)),
        )
        .await
        .unwrap();

        let report = consolidate_agent(&actor, 0).await.unwrap();
        assert_eq!(report.tokens_used, 21);
        let measured = get_agent_measured_token_usage(&actor).await.unwrap();
        assert!(measured.complete);
        assert_eq!(measured.usage.input_tokens, 10);
        assert_eq!(measured.usage.output_tokens, 4);
        assert_eq!(measured.usage.reasoning_tokens, Some(7));
        actor.stop(None);
        handle.await.unwrap();

        let restored_behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("[]", 1, 1)), simple_counter())
                .with_semantic_memory(semantic)
                .with_core_memory(core, std::collections::HashMap::new())
                .with_checkpoint_store(checkpoints);
        let (restored_actor, restored_handle) = AgentActor::spawn(
            Some("consolidation-usage-restored".to_string()),
            AgentActor,
            (config, Box::new(restored_behavior)),
        )
        .await
        .unwrap();
        let restored = get_agent_measured_token_usage(&restored_actor)
            .await
            .unwrap();
        assert_eq!(restored, measured);
        restored_actor.stop(None);
        restored_handle.await.unwrap();
    }

    #[tokio::test]
    async fn consolidation_transport_failure_persists_sticky_unknown_usage() {
        use crate::actor_impl::{consolidate_agent, get_agent_measured_token_usage, AgentActor};
        use axocoatl_memory::{CheckpointPolicy, CheckpointStore};
        use ractor::Actor;

        let dir = tempfile::tempdir().unwrap();
        let semantic = hashed_semantic(dir.path(), "durable fact to consolidate");
        let mut core =
            axocoatl_memory::CoreMemoryStore::new("a", dir.path().join("core-error.json"));
        core.ensure_block(axocoatl_memory::MemoryBlock::new("human", 0));
        let core = Arc::new(tokio::sync::RwLock::new(core));
        let checkpoints = Arc::new(CheckpointStore::new(
            dir.path().join("checkpoints-error"),
            CheckpointPolicy::Manual,
        ));
        let config = AgentConfig {
            id: AgentId::new("consolidation-error"),
            ..AgentConfig::default()
        };
        let behavior = DefaultAgentBehavior::new(Arc::new(FailingLlm), simple_counter())
            .with_semantic_memory(semantic.clone())
            .with_core_memory(core.clone(), std::collections::HashMap::new())
            .with_checkpoint_store(checkpoints.clone());
        let (actor, handle) = AgentActor::spawn(
            Some("consolidation-error-first".to_string()),
            AgentActor,
            (config.clone(), Box::new(behavior)),
        )
        .await
        .unwrap();

        let error = consolidate_agent(&actor, 0).await.unwrap_err();
        assert!(error.contains("mock LLM failure"));
        let measured = get_agent_measured_token_usage(&actor).await.unwrap();
        assert!(!measured.complete);
        assert_eq!(measured.usage, TokenUsageStats::default());
        let checkpoint = checkpoints.load_latest(&config.id).await.unwrap().unwrap();
        assert!(!checkpoint.cumulative_token_usage_known);
        actor.stop(None);
        handle.await.unwrap();

        let restored_behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("[]", 1, 1)), simple_counter())
                .with_semantic_memory(semantic)
                .with_core_memory(core, std::collections::HashMap::new())
                .with_checkpoint_store(checkpoints);
        let (restored_actor, restored_handle) = AgentActor::spawn(
            Some("consolidation-error-restored".to_string()),
            AgentActor,
            (config, Box::new(restored_behavior)),
        )
        .await
        .unwrap();
        let restored = get_agent_measured_token_usage(&restored_actor)
            .await
            .unwrap();
        assert!(!restored.complete);
        assert_eq!(restored.usage, TokenUsageStats::default());
        restored_actor.stop(None);
        restored_handle.await.unwrap();
    }

    #[tokio::test]
    async fn on_stop_never_dispatches_consolidation_provider_work() {
        let dir = tempfile::tempdir().unwrap();
        let semantic = hashed_semantic(dir.path(), "durable fact that could be consolidated");
        let mut core =
            axocoatl_memory::CoreMemoryStore::new("a", dir.path().join("stop-core.json"));
        core.ensure_block(axocoatl_memory::MemoryBlock::new("human", 0));
        let core = Arc::new(tokio::sync::RwLock::new(core));
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: Some(TokenUsageStats::new(10, 1)),
            tool_first: false,
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_semantic_memory(semantic)
            .with_core_memory(core.clone(), std::collections::HashMap::new());
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        behavior.on_stop().await.unwrap();

        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(core
            .read()
            .await
            .block("human")
            .expect("seeded core block")
            .value
            .is_empty());
    }

    #[tokio::test]
    async fn consolidation_reserves_output_and_fails_before_provider_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let semantic = hashed_semantic(dir.path(), "durable fact to consolidate");
        let mut core =
            axocoatl_memory::CoreMemoryStore::new("a", dir.path().join("bounded-core.json"));
        core.ensure_block(axocoatl_memory::MemoryBlock::new("human", 0));
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: Some(TokenUsageStats::new(10, 1)),
            tool_first: false,
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_semantic_memory(semantic)
            .with_core_memory(
                Arc::new(tokio::sync::RwLock::new(core)),
                std::collections::HashMap::new(),
            );
        behavior
            .on_start(&test_config_with_budget(100))
            .await
            .unwrap();

        let error = behavior.on_consolidate().await.unwrap_err();
        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let measured = behavior.cumulative_token_usage_measurement();
        assert!(measured.complete);
        assert_eq!(measured.usage, TokenUsageStats::default());
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

        // Enforcement resets per Execute, while lifetime reporting accumulates.
        let tracker = behavior.tracker.as_ref().unwrap();
        assert_eq!(tracker.total_used(), 150);
        assert_eq!(behavior.cumulative_token_usage_snapshot().total(), 300);
    }

    #[tokio::test]
    async fn default_behavior_budget_abort() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured,
            reported_usage: Some(TokenUsageStats::new(10, 10)),
            tool_first: false,
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
        let mut config = test_config_with_budget(70);
        config.sampling.max_tokens = Some(50);
        behavior.on_start(&config).await.unwrap();

        behavior.execute(AgentInput::text("first")).await.unwrap();
        behavior.execute(AgentInput::text("second")).await.unwrap();
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "each independent Execute gets a fresh per-execution allowance"
        );
    }

    #[tokio::test]
    async fn unset_output_limit_is_capped_to_known_model_and_remaining_abort_budget() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
            reported_usage: Some(TokenUsageStats::new(10, 1)),
            tool_first: false,
        });
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior
            .on_start(&test_config_with_budget(100))
            .await
            .unwrap();

        behavior.execute(AgentInput::text("bounded")).await.unwrap();
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].max_tokens,
            Some(90),
            "the provider's 1000-token default must be reduced to local safe headroom"
        );
    }

    #[tokio::test]
    async fn stateless_abort_budget_rejects_before_provider_dispatch() {
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: Some(TokenUsageStats::new(10, 1)),
            tool_first: false,
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
        let mut config = test_config_with_budget(50);
        config.sampling.max_tokens = Some(50);
        behavior.on_start(&config).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("stateless").with_stateless(true))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provider_reported_budget_overrun_fails_current_call() {
        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: Some(TokenUsageStats::new(50, 20)),
            tool_first: false,
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
        let mut config = test_config_with_budget(60);
        config.sampling.max_tokens = Some(50);
        behavior.on_start(&config).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("provider undercount seam"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::TokenBudgetExceeded {
                used: 70,
                budget: 60
            }
        ));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn hostile_provider_usage_saturates_and_cannot_bypass_abort_budget() {
        for reported_usage in [
            TokenUsageStats::new(usize::MAX, 1),
            TokenUsageStats::new(0, 0).with_reasoning(usize::MAX),
        ] {
            let provider = Arc::new(BudgetProbeLlm {
                calls: std::sync::atomic::AtomicUsize::new(0),
                captured: Arc::new(std::sync::Mutex::new(Vec::new())),
                reported_usage: Some(reported_usage),
                tool_first: false,
            });
            let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
            let mut config = test_config_with_budget(100);
            config.sampling.max_tokens = Some(50);
            behavior.on_start(&config).await.unwrap();

            let error = behavior
                .execute(AgentInput::text("hostile usage"))
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                AgentError::TokenBudgetExceeded {
                    used: usize::MAX,
                    budget: 100
                }
            ));
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(behavior.tracker.as_ref().unwrap().total_used(), usize::MAX);
        }
    }

    #[tokio::test]
    async fn no_usage_tool_output_is_counted_before_followup_budget_preflight() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let provider = Arc::new(BudgetProbeLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            reported_usage: None,
            tool_first: true,
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_tool_executor(Arc::new(executor));
        let mut config = test_config_with_budget(100);
        config.sampling.max_tokens = Some(30);
        behavior.on_start(&config).await.unwrap();

        let error = behavior
            .execute(AgentInput::text("run the generated tool call"))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "serialized generated tool identity/name/arguments must exhaust headroom before follow-up"
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

    struct PerRequestCapsLlm {
        stream_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for PerRequestCapsLlm {
        fn provider_id(&self) -> &str {
            "per-request-caps"
        }

        fn model_id(&self) -> &str {
            "large"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                vision: true,
                max_context_tokens: 50_000,
                max_output_tokens: 1_000,
                ..Default::default()
            }
        }

        fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
            match request.model_override.as_deref() {
                Some("tiny") => ProviderCapabilities {
                    streaming: true,
                    max_context_tokens: 200,
                    max_output_tokens: 20,
                    ..Default::default()
                },
                _ => self.capabilities(),
            }
        }

        fn model_constraints_known(&self, request: &ChatRequest) -> bool {
            request.model_override.as_deref() != Some("unknown")
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse {
                content: "summary".to_string(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::default(),
                model: "large".to_string(),
                provider: "per-request-caps".to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    delta: "ok".to_string(),
                }),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    fn approximate_counter() -> Arc<dyn TokenCounter> {
        Arc::new(axocoatl_token::ApproximateCounter::new().unwrap())
    }

    #[tokio::test]
    async fn context_limit_uses_effective_model_and_skips_unknown_constraints() {
        let tiny_provider = Arc::new(PerRequestCapsLlm {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut tiny = DefaultAgentBehavior::new(tiny_provider.clone(), approximate_counter());
        let error = tiny
            .execute(
                AgentInput::text("word ".repeat(400)).with_model_override(Some("tiny".to_string())),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::ContextLimitExceeded { .. }));
        assert_eq!(
            tiny_provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        let large_provider = Arc::new(PerRequestCapsLlm {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut large = DefaultAgentBehavior::new(large_provider.clone(), approximate_counter());
        large
            .execute(
                AgentInput::text("word ".repeat(400))
                    .with_model_override(Some("large".to_string())),
            )
            .await
            .unwrap();
        assert_eq!(
            large_provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let unknown_provider = Arc::new(PerRequestCapsLlm {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut unknown =
            DefaultAgentBehavior::new(unknown_provider.clone(), approximate_counter());
        unknown
            .execute(
                AgentInput::text("word ".repeat(20_000))
                    .with_model_override(Some("unknown".to_string())),
            )
            .await
            .unwrap();
        assert_eq!(
            unknown_provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    struct PinnedRouteCapsLlm {
        stream_calls: std::sync::atomic::AtomicUsize,
    }

    impl PinnedRouteCapsLlm {
        fn routed_model(request: &ChatRequest) -> Option<&str> {
            request
                .messages
                .iter()
                .flat_map(|message| &message.tool_calls)
                .rev()
                .find_map(|call| {
                    call.provider_metadata
                        .get(axocoatl_llm::TOOL_METADATA_ROUTE_MODEL)
                        .map(String::as_str)
                })
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for PinnedRouteCapsLlm {
        fn provider_id(&self) -> &str {
            "same-provider"
        }

        fn model_id(&self) -> &str {
            "large-primary"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                max_context_tokens: 20_000,
                max_output_tokens: 100,
                ..Default::default()
            }
        }

        fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
            if Self::routed_model(request) == Some("tiny-fallback") {
                ProviderCapabilities {
                    streaming: true,
                    tool_calling: true,
                    max_context_tokens: 500,
                    max_output_tokens: 50,
                    ..Default::default()
                }
            } else {
                self.capabilities()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!("pinned-route seam uses streaming")
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let call = self
                .stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if call == 0 {
                vec![
                    Ok(StreamEvent::ProviderRoute {
                        metadata: axocoatl_core::ProviderMetadata::from([
                            (
                                axocoatl_llm::TOOL_METADATA_ROUTE_PROVIDER.to_string(),
                                "same-provider".to_string(),
                            ),
                            (
                                axocoatl_llm::TOOL_METADATA_ROUTE_MODEL.to_string(),
                                "tiny-fallback".to_string(),
                            ),
                        ]),
                    }),
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "fallback-call".to_string(),
                        name: Some("echo".to_string()),
                        args_delta: "{\"text\":\"ok\"}".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                })]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn tiny_pinned_fallback_route_rejects_tool_followup_before_second_stream() {
        use axocoatl_tools::EchoTool;

        let provider = Arc::new(PinnedRouteCapsLlm {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), approximate_counter())
            .with_tool_executor(Arc::new(executor));

        let error = behavior
            .execute(AgentInput::text("word ".repeat(1_000)))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::ContextLimitExceeded { .. }));
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the routed tiny-model followup must fail before a second provider stream"
        );
        let messages = behavior.session.as_chat_messages();
        let routed_call = messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|call| call.id == "fallback-call")
            .unwrap();
        assert_eq!(
            routed_call
                .provider_metadata
                .get(axocoatl_llm::TOOL_METADATA_ROUTE_MODEL)
                .map(String::as_str),
            Some("tiny-fallback")
        );
    }

    #[tokio::test]
    async fn realistic_image_transport_passes_but_huge_extracted_text_fails_locally() {
        let image_provider = Arc::new(PerRequestCapsLlm {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut image_behavior =
            DefaultAgentBehavior::new(image_provider.clone(), approximate_counter());
        let image = axocoatl_core::AgentAttachment {
            id: "image-1".to_string(),
            name: "photo.png".to_string(),
            mime: "image/png".to_string(),
            bytes: vec![0_u8; 5 * 1024 * 1024],
            size: 5 * 1024 * 1024,
            extracted_text: None,
        };
        image_behavior
            .execute(
                AgentInput::text("inspect")
                    .with_model_override(Some("large".to_string()))
                    .with_attachments(vec![image]),
            )
            .await
            .unwrap();
        assert_eq!(
            image_provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let text_provider = Arc::new(PerRequestCapsLlm {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut text_behavior =
            DefaultAgentBehavior::new(text_provider.clone(), approximate_counter());
        let document = axocoatl_core::AgentAttachment {
            id: "doc-1".to_string(),
            name: "huge.txt".to_string(),
            mime: "text/plain".to_string(),
            bytes: Vec::new(),
            size: 500_000,
            extracted_text: Some("word ".repeat(60_000)),
        };
        let error = text_behavior
            .execute(
                AgentInput::text("inspect")
                    .with_model_override(Some("large".to_string()))
                    .with_attachments(vec![document]),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::ContextLimitExceeded { .. }));
        assert_eq!(
            text_provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    struct CompressionProbeLlm {
        chat_calls: std::sync::atomic::AtomicUsize,
        stream_calls: std::sync::atomic::AtomicUsize,
        max_context_tokens: usize,
    }

    struct AccountingCompressionLlm {
        chat_calls: std::sync::atomic::AtomicUsize,
        stream_calls: std::sync::atomic::AtomicUsize,
        summary: String,
        summary_usage: TokenUsageStats,
        main_usage: TokenUsageStats,
    }

    #[async_trait::async_trait]
    impl LlmProvider for AccountingCompressionLlm {
        fn provider_id(&self) -> &str {
            "accounting-compression"
        }

        fn model_id(&self) -> &str {
            "accounting-compression-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                max_context_tokens: 400,
                max_output_tokens: 0,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.chat_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ChatResponse {
                content: self.summary.clone(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: self.summary_usage.clone(),
                model: self.model_id().to_string(),
                provider: self.provider_id().to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    delta: "main final".to_string(),
                }),
                Ok(StreamEvent::Usage(self.main_usage.clone())),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    fn seed_large_completed_prefix(behavior: &mut DefaultAgentBehavior) {
        for index in 0..20 {
            behavior.session.append(
                MessageRole::User,
                format!("old-user-{index} {}", "x".repeat(600)),
                151,
            );
            behavior.session.append(
                MessageRole::Assistant,
                format!("old-answer-{index} {}", "y".repeat(600)),
                151,
            );
        }
    }

    #[tokio::test]
    async fn no_budget_compaction_and_main_usage_merge_once_with_reasoning() {
        let provider = Arc::new(AccountingCompressionLlm {
            chat_calls: std::sync::atomic::AtomicUsize::new(0),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            summary: "compact completed-prefix summary".to_string(),
            summary_usage: TokenUsageStats::new(3, 2).with_reasoning(5),
            main_usage: TokenUsageStats::new(7, 4).with_reasoning(6),
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        seed_large_completed_prefix(&mut behavior);

        let output = behavior
            .execute(AgentInput::text("current compacted turn"))
            .await
            .unwrap();

        assert_eq!(
            provider
                .chat_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the old completed prefix must reach the shipped LLM summary stage"
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(output.token_usage.input_tokens, 10);
        assert_eq!(output.token_usage.output_tokens, 6);
        assert_eq!(output.token_usage.reasoning_tokens, Some(11));
        let lifetime = behavior.cumulative_token_usage_measurement();
        assert!(lifetime.complete);
        assert_eq!(lifetime.usage, output.token_usage);
        assert_eq!(
            behavior.last_execution_token_usage(),
            Some(output.token_usage)
        );
    }

    #[tokio::test]
    async fn failed_paid_compaction_usage_is_checkpointed_and_restored() {
        use axocoatl_memory::{CheckpointPolicy, CheckpointStore};

        let dir = tempfile::tempdir().unwrap();
        let checkpoints = Arc::new(CheckpointStore::new(dir.path(), CheckpointPolicy::Manual));
        let provider = Arc::new(AccountingCompressionLlm {
            chat_calls: std::sync::atomic::AtomicUsize::new(0),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            summary: String::new(),
            summary_usage: TokenUsageStats::new(3, 2).with_reasoning(5),
            main_usage: TokenUsageStats::new(7, 4).with_reasoning(6),
        });
        let config = AgentConfig {
            id: AgentId::new("failed-paid-compaction"),
            ..AgentConfig::default()
        };
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_checkpoint_store(checkpoints.clone());
        behavior.on_start(&config).await.unwrap();
        seed_large_completed_prefix(&mut behavior);
        let complete_prefix = behavior.session.messages().to_vec();

        behavior
            .execute(AgentInput::text("current turn must not be orphaned"))
            .await
            .unwrap_err();

        assert_eq!(
            provider
                .chat_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            behavior.last_execution_token_usage(),
            Some(TokenUsageStats::new(3, 2).with_reasoning(5))
        );
        let checkpoint = checkpoints.load_latest(&config.id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_vec(&checkpoint.session_messages).unwrap(),
            serde_json::to_vec(&complete_prefix).unwrap()
        );
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(3, 2).with_reasoning(5)
        );
        assert!(checkpoint.cumulative_token_usage_known);

        let mut restored =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("ok", 1, 1)), simple_counter())
                .with_checkpoint_store(checkpoints);
        restored.on_start(&config).await.unwrap();
        let restored_usage = restored.cumulative_token_usage_measurement();
        assert!(restored_usage.complete);
        assert_eq!(restored_usage.usage, checkpoint.cumulative_token_usage);
        assert_eq!(
            serde_json::to_vec(restored.session.messages()).unwrap(),
            serde_json::to_vec(&checkpoint.session_messages).unwrap()
        );
    }

    #[async_trait::async_trait]
    impl LlmProvider for CompressionProbeLlm {
        fn provider_id(&self) -> &str {
            "compression-probe"
        }

        fn model_id(&self) -> &str {
            "compression-probe-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                max_context_tokens: self.max_context_tokens,
                max_output_tokens: 0,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.chat_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ChatResponse {
                content: "compact completed-prefix summary".to_string(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::default(),
                model: "compression-probe-model".to_string(),
                provider: "compression-probe".to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(tokio_stream::iter(vec![Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            })])))
        }
    }

    fn compression_tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "repo_read".to_string(),
            arguments: serde_json::json!({"path": format!("/{id}")}),
            provider_metadata: axocoatl_core::ProviderMetadata::from([(
                "gemini.thought_signature".to_string(),
                format!("signature-{id}"),
            )]),
        }
    }

    fn active_compression_suffix() -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user("CURRENT_USER")];
        for index in 0..7 {
            let call = compression_tool_call(&format!("active-{index}"));
            messages.push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![call.clone()],
            ));
            messages.push(ChatMessage::tool_result(
                format!("result-{index}"),
                &call.name,
                &call.id,
            ));
        }
        messages.push(ChatMessage::assistant("CURRENT_DONE"));
        messages
    }

    #[tokio::test]
    async fn persistent_compaction_returns_followup_boundary_and_archives_structured_tools() {
        let provider = Arc::new(CompressionProbeLlm {
            chat_calls: std::sync::atomic::AtomicUsize::new(0),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            max_context_tokens: 400,
        });
        let data = tempfile::tempdir().unwrap();
        let daily_log = Arc::new(axocoatl_memory::DailyLogMemory::new(
            "compression-archive",
            data.path(),
        ));
        let mut behavior =
            DefaultAgentBehavior::new(provider, simple_counter()).with_daily_log(daily_log.clone());

        let mut messages = Vec::new();
        for index in 0..20 {
            messages.push(ChatMessage::user(format!(
                "old-user-{index} {}",
                "x".repeat(600)
            )));
            messages.push(ChatMessage::assistant(format!(
                "old-answer-{index} {}",
                "y".repeat(600)
            )));
        }
        let archived_call = compression_tool_call("archived-call");
        messages.push(ChatMessage::user("old tool turn"));
        messages.push(ChatMessage::assistant_with_tool_calls(
            "",
            vec![archived_call.clone()],
        ));
        messages.push(ChatMessage::tool_result(
            "archived result",
            &archived_call.name,
            &archived_call.id,
        ));
        messages.push(ChatMessage::assistant("old tool complete"));
        let old_boundary = messages.len();
        let active = active_compression_suffix();
        assert!(active.len() > 12);
        let active_json = serde_json::to_vec(&active).unwrap();
        messages.extend(active);
        behavior
            .session
            .replace_with_chat_messages(&messages, |text| text.len() / 4 + 1);

        let (new_boundary, _summary_usage) = behavior
            .compact_session(old_boundary, None, None, 0)
            .await
            .unwrap();
        assert!(new_boundary < old_boundary);
        let compacted = behavior.session.as_chat_messages();
        assert_eq!(
            serde_json::to_vec(&compacted[new_boundary..]).unwrap(),
            active_json
        );

        let followup = behavior
            .build_request_from_session(None, None, new_boundary, 0)
            .unwrap();
        let session_start = followup.messages.len() - compacted.len();
        assert_eq!(
            serde_json::to_vec(&followup.messages[session_start + new_boundary..]).unwrap(),
            active_json
        );

        let today = chrono::Local::now().date_naive();
        let entries = daily_log.read_range(today, today).await.unwrap();
        let archive = entries
            .iter()
            .find(|entry| entry.content["reason"] == "context_compaction")
            .expect("compaction archive entry");
        let archived_messages: Vec<ChatMessage> =
            serde_json::from_value(archive.content["messages"].clone()).unwrap();
        let replayed_call = archived_messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|call| call.id == "archived-call")
            .unwrap();
        assert_eq!(replayed_call.arguments["path"], "/archived-call");
        assert_eq!(
            replayed_call
                .provider_metadata
                .get("gemini.thought_signature")
                .map(String::as_str),
            Some("signature-archived-call")
        );
    }

    #[tokio::test]
    async fn configured_archive_failure_keeps_session_and_skips_summarizer_provider() {
        let provider = Arc::new(CompressionProbeLlm {
            chat_calls: std::sync::atomic::AtomicUsize::new(0),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            max_context_tokens: 200,
        });
        let data = tempfile::tempdir().unwrap();
        let blocked_root = data.path().join("not-a-directory");
        std::fs::write(&blocked_root, "blocker").unwrap();
        let daily_log = Arc::new(axocoatl_memory::DailyLogMemory::new(
            "archive-failure",
            &blocked_root,
        ));
        let mut behavior =
            DefaultAgentBehavior::new(provider.clone(), simple_counter()).with_daily_log(daily_log);
        for index in 0..30 {
            behavior.session.append(
                MessageRole::User,
                format!("old-{index} {}", "x".repeat(600)),
                151,
            );
            behavior.session.append(
                MessageRole::Assistant,
                format!("answer-{index} {}", "y".repeat(600)),
                151,
            );
        }
        let turn_start = behavior.session.len();
        behavior.session.append(MessageRole::User, "CURRENT", 2);
        let before = serde_json::to_vec(&behavior.session.as_chat_messages()).unwrap();

        let error = behavior
            .compact_session(turn_start, None, None, 0)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("failed to archive structured"));
        assert_eq!(
            serde_json::to_vec(&behavior.session.as_chat_messages()).unwrap(),
            before
        );
        assert_eq!(
            provider
                .chat_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    struct PendingCompressionLlm {
        summary_started: Arc<tokio::sync::Notify>,
        stream_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for PendingCompressionLlm {
        fn provider_id(&self) -> &str {
            "pending-compression"
        }

        fn model_id(&self) -> &str {
            "pending-compression-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                max_context_tokens: 200,
                max_output_tokens: 0,
                ..Default::default()
            }
        }

        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.summary_started.notify_one();
            std::future::pending().await
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(tokio_stream::pending()))
        }
    }

    #[tokio::test]
    async fn cancellation_during_async_compaction_keeps_session_and_skips_main_provider_call() {
        use crate::run_control::{AgentRunControl, AgentRunId, AgentRunOutcome};

        let started = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(PendingCompressionLlm {
            summary_started: started.clone(),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter());
        for index in 0..30 {
            behavior.session.append(
                MessageRole::User,
                format!("old-{index} {}", "x".repeat(1_000)),
                251,
            );
            behavior.session.append(
                MessageRole::Assistant,
                format!("answer-{index} {}", "y".repeat(1_000)),
                251,
            );
        }
        let before = behavior.session.as_chat_messages();
        let control = AgentRunControl::new(AgentRunId::new("cancel-compaction"));
        let outcome = {
            let execution =
                behavior.execute_controlled(AgentInput::text("CURRENT"), control.clone());
            tokio::pin!(execution);

            tokio::select! {
                _ = started.notified() => {}
                result = &mut execution => panic!("execution finished before compaction cancellation: {result:?}"),
            }
            control.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut execution)
                .await
                .expect("cancelled compaction did not finish")
                .unwrap()
        };
        assert!(matches!(outcome, AgentRunOutcome::Cancelled { .. }));

        let mut expected = before;
        expected.push(ChatMessage::user("CURRENT"));
        assert_eq!(
            serde_json::to_vec(&behavior.session.as_chat_messages()).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn abort_is_default_policy() {
        // A configured spend budget is enforced by default — overflow aborts.
        assert_eq!(OverflowPolicy::default(), OverflowPolicy::Abort);
    }

    #[tokio::test]
    async fn warn_policy_continues_where_abort_fails() {
        // A single provider response reports above the 120-token per-call and
        // per-execution allowance. Abort fails that same activation after the
        // exact usage arrives; Warn records it and continues.
        let mut abort =
            DefaultAgentBehavior::new(Arc::new(CtxLlm { per_call: 130 }), simple_counter());
        abort.on_start(&test_config_with_budget(120)).await.unwrap();
        let second_abort = abort.execute(AgentInput::text("t1")).await;
        assert!(
            matches!(second_abort, Err(AgentError::TokenBudgetExceeded { .. })),
            "abort should surface the reported overrun, got {second_abort:?}"
        );

        // Warn policy → the same overflow logs and continues past the local
        // guard; context compaction is independent of it.
        let mut cfg = test_config_with_budget(120);
        cfg.token_budget.as_mut().unwrap().overflow_policy = OverflowPolicy::Warn;
        let mut warn =
            DefaultAgentBehavior::new(Arc::new(CtxLlm { per_call: 130 }), simple_counter());
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
        // Regression for the lightweight-chat API's per-chat system prompt.
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

        let with_override = behavior
            .build_request_from_session(Some("Respond in haiku."), None, 0, 0)
            .unwrap();
        let with_default = behavior
            .build_request_from_session(None, None, 0, 0)
            .unwrap();

        let sys_override = with_override.messages[0].text_content().unwrap();
        let sys_default = with_default.messages[0].text_content().unwrap();

        assert!(sys_override.contains("Respond in haiku."));
        assert!(!sys_override.contains("Default prompt."));
        assert!(sys_default.contains("Default prompt."));
    }

    /// Records every request it receives; returns a one-token text answer.
    struct CapturingLlm {
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CapturingLlm {
        fn provider_id(&self) -> &str {
            "capture"
        }
        fn model_id(&self) -> &str {
            "capture-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
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
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    delta: "ok".to_string(),
                }),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    /// Captures requests and holds the first provider call at a gate. Used to
    /// prove that dropping the caller waiting on an actor reply does not leave
    /// the actor's canonical session swapped to a caller-owned chat transcript.
    struct GatedCapturingLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        first_started: Arc<tokio::sync::Notify>,
        release_first: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for GatedCapturingLlm {
        fn provider_id(&self) -> &str {
            "gated-capture"
        }

        fn model_id(&self) -> &str {
            "gated-capture-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
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
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
            }
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    delta: "ok".to_string(),
                }),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn execute_applies_input_system_override() {
        // Reproduces axocoatl#64: the per-request `system_override` on the
        // agent-execute path must replace the configured prompt for that call.
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingLlm {
            captured: captured.clone(),
        });
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior
            .on_start(&AgentConfig {
                system_prompt: Some("CONFIGURED TAXONOMY".to_string()),
                ..AgentConfig::default()
            })
            .await
            .unwrap();

        behavior
            .execute(
                AgentInput::text("hi").with_system_override(Some("LABEL SPAM OR HAM".to_string())),
            )
            .await
            .unwrap();

        let reqs = captured.lock().unwrap();
        let sys = reqs[0].messages[0].text_content().unwrap();
        assert!(sys.contains("LABEL SPAM OR HAM"), "system sent was: {sys}");
        assert!(
            !sys.contains("CONFIGURED TAXONOMY"),
            "configured prompt leaked through: {sys}"
        );
    }

    #[tokio::test]
    async fn stateless_execute_isolates_from_session() {
        // axocoatl#64: a stateless call builds from the input alone — override
        // wins, prior session turns don't leak in, and the call doesn't persist.
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingLlm {
            captured: captured.clone(),
        });
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior
            .on_start(&AgentConfig {
                system_prompt: Some("CONFIGURED".to_string()),
                ..AgentConfig::default()
            })
            .await
            .unwrap();

        // Pollute the session with a prior turn.
        behavior
            .session
            .append(MessageRole::User, "earlier question", 2);
        let len_before = behavior.session().len();

        behavior
            .execute(
                AgentInput::text("classify this")
                    .with_system_override(Some("OVERRIDE".to_string()))
                    .with_stateless(true),
            )
            .await
            .unwrap();

        let reqs = captured.lock().unwrap();
        let msgs = &reqs[0].messages;
        // Exactly [system(override), user(content)] — no configured prompt, no
        // prior session turn.
        assert_eq!(msgs.len(), 2, "expected only system + user, got {msgs:?}");
        assert!(msgs[0].text_content().unwrap().contains("OVERRIDE"));
        assert!(!msgs[0].text_content().unwrap().contains("CONFIGURED"));
        assert_eq!(msgs[1].text_content(), Some("classify this"));
        assert!(!msgs
            .iter()
            .any(|m| m.text_content() == Some("earlier question")));
        drop(reqs);

        // The stateless call did not write to the session.
        assert_eq!(behavior.session().len(), len_before);
    }

    #[tokio::test]
    async fn stateless_single_inference_never_advertises_tools() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingLlm {
            captured: captured.clone(),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let output = behavior
            .execute(AgentInput::text("answer directly").with_stateless(true))
            .await
            .unwrap();

        assert_eq!(output.content, "ok");
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].tools.is_empty(),
            "single-shot stateless execution cannot advertise tools it will not execute"
        );
    }

    #[test]
    fn stateless_response_format_override_wins_for_one_request() {
        let provider = Arc::new(MockLlm::new("ok", 1, 1));
        let behavior = DefaultAgentBehavior::new(provider, simple_counter());
        let request = behavior.build_stateless_request(
            &AgentInput::text("return a schema")
                .with_response_format_override(Some(axocoatl_core::ResponseFormat::Json))
                .with_reasoning_disabled(true),
        );
        assert_eq!(
            request.response_format,
            Some(axocoatl_core::ResponseFormat::Json)
        );
        assert_eq!(
            request.provider_options,
            Some(serde_json::json!({"reasoning_effort": "none"}))
        );

        let ordinary = behavior.build_stateless_request(&AgentInput::text("answer normally"));
        assert_eq!(ordinary.response_format, None);
        assert_eq!(ordinary.provider_options, None);
    }

    #[tokio::test]
    async fn supplied_history_is_call_local_and_persists_only_usage_accounting() {
        use axocoatl_memory::CheckpointPolicy;

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingLlm {
            captured: captured.clone(),
        });
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(CheckpointStore::new(
            tmp.path(),
            CheckpointPolicy::EveryLlmCall,
        ));
        let config = AgentConfig {
            id: AgentId::new("shared-agent"),
            name: "Shared Agent".to_string(),
            system_prompt: Some("SYSTEM".to_string()),
            ..AgentConfig::default()
        };
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_checkpoint_store(store.clone());
        behavior.on_start(&config).await.unwrap();

        // Establish the configured agent's canonical transcript and checkpoint.
        behavior
            .execute(AgentInput::text("GLOBAL_POISON"))
            .await
            .unwrap();
        let canonical_before = serde_json::to_value(behavior.session().messages()).unwrap();
        assert_eq!(behavior.checkpoint_version, 1);

        behavior
            .execute(AgentInput::text("A_NEXT").with_supplied_history(vec![
                ChatMessage::user("A_SEED"),
                ChatMessage::assistant("A_REPLY"),
            ]))
            .await
            .unwrap();
        // Empty history is an explicit, meaningful new chat. It must not fall
        // back to the configured agent's canonical session.
        behavior
            .execute(AgentInput::text("B_FIRST").with_supplied_history(Vec::new()))
            .await
            .unwrap();
        // A fork replays only the prefix its ChatStore record supplied.
        behavior
            .execute(AgentInput::text("CHILD_NEXT").with_supplied_history(vec![
                ChatMessage::user("A_SEED"),
                ChatMessage::assistant("A_REPLY"),
            ]))
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_value(behavior.session().messages()).unwrap(),
            canonical_before,
            "caller-owned turns must not mutate the configured agent transcript"
        );
        assert_eq!(behavior.checkpoint_version, 4);
        let checkpoint = store
            .load_latest(&AgentId::new("shared-agent"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.version, 4);
        let checkpoint_text = checkpoint
            .session_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(checkpoint_text, vec!["GLOBAL_POISON", "ok"]);
        assert_eq!(
            checkpoint.cumulative_token_usage.total(),
            behavior.cumulative_token_usage_snapshot().total()
        );

        let mut restored = DefaultAgentBehavior::new(
            Arc::new(CapturingLlm {
                captured: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            simple_counter(),
        )
        .with_checkpoint_store(store.clone());
        restored.on_start(&config).await.unwrap();
        assert_eq!(
            restored.cumulative_token_usage_snapshot().total(),
            checkpoint.cumulative_token_usage.total(),
            "request-local paid calls remain visible after actor recreation"
        );
        assert_eq!(
            serde_json::to_value(restored.session().messages()).unwrap(),
            canonical_before,
            "accounting-only checkpoints never persist caller-owned history"
        );

        // A later canonical turn resumes the configured agent transcript and
        // still excludes every caller-owned chat/fork turn.
        behavior
            .execute(AgentInput::text("GLOBAL_NEXT"))
            .await
            .unwrap();

        let request_texts = captured
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .map(|message| message.text_content().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(request_texts[0], vec!["SYSTEM", "GLOBAL_POISON"]);
        assert_eq!(
            request_texts[1],
            vec!["SYSTEM", "A_SEED", "A_REPLY", "A_NEXT"]
        );
        assert_eq!(request_texts[2], vec!["SYSTEM", "B_FIRST"]);
        assert_eq!(
            request_texts[3],
            vec!["SYSTEM", "A_SEED", "A_REPLY", "CHILD_NEXT"]
        );
        assert_eq!(
            request_texts[4],
            vec!["SYSTEM", "GLOBAL_POISON", "ok", "GLOBAL_NEXT"]
        );

        assert_eq!(behavior.checkpoint_version, 5);
        let checkpoint = store
            .load_latest(&AgentId::new("shared-agent"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.version, 5);
        assert!(checkpoint
            .session_messages
            .iter()
            .all(|message| !message.content.contains("A_")
                && !message.content.contains("B_")
                && !message.content.contains("CHILD_")));
    }

    #[tokio::test]
    async fn supplied_history_restores_actor_session_after_provider_error() {
        let mut behavior = DefaultAgentBehavior::new(Arc::new(FailingLlm), simple_counter());
        behavior.on_start(&AgentConfig::default()).await.unwrap();
        behavior.session.append(MessageRole::User, "GLOBAL_SAFE", 3);
        let canonical_before = serde_json::to_value(behavior.session().messages()).unwrap();

        let result = behavior
            .execute(
                AgentInput::text("CHAT_FAIL")
                    .with_supplied_history(vec![ChatMessage::user("CHAT_SEED")]),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(
            serde_json::to_value(behavior.session().messages()).unwrap(),
            canonical_before
        );
    }

    #[tokio::test]
    async fn supplied_history_restores_actor_session_after_budget_abort() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingLlm {
            captured: captured.clone(),
        });
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior
            .on_start(&AgentConfig {
                token_budget: Some(TokenBudget {
                    per_call: 1,
                    per_execution: 1,
                    overflow_policy: OverflowPolicy::Abort,
                }),
                ..AgentConfig::default()
            })
            .await
            .unwrap();
        behavior.session.append(MessageRole::User, "GLOBAL_SAFE", 3);
        let canonical_before = serde_json::to_value(behavior.session().messages()).unwrap();

        let result = behavior
            .execute(
                AgentInput::text("CHAT_INPUT_THAT_EXCEEDS_ONE_TOKEN")
                    .with_supplied_history(vec![ChatMessage::user("CHAT_SEED")]),
            )
            .await;

        assert!(matches!(
            result,
            Err(AgentError::TokenBudgetExceeded { .. })
        ));
        assert_eq!(
            serde_json::to_value(behavior.session().messages()).unwrap(),
            canonical_before
        );
        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn supplied_history_restores_actor_session_when_reply_waiter_is_cancelled() {
        use crate::actor_impl::{execute_agent, execute_agent_streaming, AgentActor};
        use ractor::Actor;

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let first_started = Arc::new(tokio::sync::Notify::new());
        let release_first = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(GatedCapturingLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
            first_started: first_started.clone(),
            release_first: release_first.clone(),
        });
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior.session.append(MessageRole::User, "GLOBAL_SAFE", 3);
        let config = AgentConfig {
            id: AgentId::new("cancel-shared-agent"),
            name: "Cancel Shared Agent".to_string(),
            system_prompt: Some("SYSTEM".to_string()),
            ..AgentConfig::default()
        };
        let (actor, handle) = AgentActor::spawn(
            Some("cancel-shared-agent-test".to_string()),
            AgentActor,
            (config, Box::new(behavior) as Box<dyn AgentBehavior>),
        )
        .await
        .unwrap();

        let (sink, _chunks) = tokio::sync::mpsc::unbounded_channel();
        let actor_for_chat = actor.clone();
        let waiter = tokio::spawn(async move {
            execute_agent_streaming(
                &actor_for_chat,
                AgentInput::text("CHAT_NEXT")
                    .with_supplied_history(vec![ChatMessage::user("CHAT_SEED")]),
                sink,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), first_started.notified())
            .await
            .expect("caller-owned turn did not reach the provider");

        // Mirrors ChatStop: the socket-side waiter is dropped, while the actor
        // continues its already-enqueued Execute message to a safe boundary.
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release_first.notify_one();

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            execute_agent(&actor, AgentInput::text("GLOBAL_NEXT")),
        )
        .await
        .expect("canonical follow-up timed out")
        .unwrap();

        let request_texts = captured
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .map(|message| message.text_content().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(request_texts[0], vec!["SYSTEM", "CHAT_SEED", "CHAT_NEXT"]);
        assert_eq!(
            request_texts[1],
            vec!["SYSTEM", "GLOBAL_SAFE", "GLOBAL_NEXT"]
        );

        actor.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn supplied_history_preserves_streamed_tool_round_trip() {
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
        behavior
            .session
            .append(MessageRole::User, "GLOBAL_TOOL_POISON", 4);
        let canonical_before = serde_json::to_value(behavior.session().messages()).unwrap();

        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        behavior.set_stream_sink(Some(sink));
        let output = behavior
            .execute(AgentInput::text("CHAT_NEXT").with_supplied_history(vec![
                ChatMessage::user("CHAT_SEED"),
                ChatMessage::assistant("CHAT_REPLY"),
            ]))
            .await
            .unwrap();
        behavior.set_stream_sink(None);

        assert_eq!(output.content, "final answer");
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.token_usage.input_tokens, 28);
        assert_eq!(output.token_usage.output_tokens, 8);
        assert_eq!(
            serde_json::to_value(behavior.session().messages()).unwrap(),
            canonical_before
        );

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(request
                .messages
                .iter()
                .all(|message| message.text_content() != Some("GLOBAL_TOOL_POISON")));
        }
        let followup = &requests[1];
        let texts = followup
            .messages
            .iter()
            .filter_map(|message| message.text_content())
            .collect::<Vec<_>>();
        assert!(texts.starts_with(&["CHAT_SEED", "CHAT_REPLY", "CHAT_NEXT"]));
        let assistant = followup
            .messages
            .iter()
            .find(|message| {
                message.role == MessageRole::Assistant && !message.tool_calls.is_empty()
            })
            .expect("assistant tool-call turn must be present");
        assert_eq!(assistant.tool_calls[0].id, "call_1");
        let tool_result = followup
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("correlated tool result must be present");
        assert_eq!(tool_result.name.as_deref(), Some("echo"));
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("call_1"));
        drop(requests);

        let mut saw_started = false;
        let mut saw_result = false;
        while let Ok(chunk) = chunks.try_recv() {
            match chunk {
                crate::behavior::AgentStreamChunk::ToolCallStarted { name, .. }
                    if name == "echo" =>
                {
                    saw_started = true;
                }
                crate::behavior::AgentStreamChunk::ToolCallResult { name, .. }
                    if name == "echo" =>
                {
                    saw_result = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_started && saw_result,
            "tool frames must remain streamed"
        );
    }

    #[tokio::test]
    async fn configured_model_flows_into_model_override() {
        // The agent's configured model is sent as the per-request model so a
        // shared OpenAI-compatible provider uses it (not the provider default);
        // an explicit per-request override still wins.
        let provider = Arc::new(MockLlm::new("ok", 1, 1));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior
            .on_start(&AgentConfig {
                model: "gemma-local".to_string(),
                ..AgentConfig::default()
            })
            .await
            .unwrap();
        behavior.session.append(MessageRole::User, "hi", 1);

        // No per-request override: falls back to the configured model.
        let req = behavior
            .build_request_from_session(None, None, 0, 0)
            .unwrap();
        assert_eq!(req.model_override.as_deref(), Some("gemma-local"));

        // Per-request override wins.
        let req = behavior
            .build_request_from_session(None, Some("override-model".to_string()), 0, 0)
            .unwrap();
        assert_eq!(req.model_override.as_deref(), Some("override-model"));
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
