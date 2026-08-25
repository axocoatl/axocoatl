use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, AgentOutput, ProviderMetadata, TokenUsageStats,
};

/// Per-activation usage plus whether the total is complete. A provider call
/// marks the state unknown while in flight; adopting a completed response
/// restores the prior completeness and merges its exact/estimated usage.
#[derive(Clone)]
pub(crate) struct ExecutionUsageState {
    usage: std::sync::Arc<std::sync::Mutex<TokenUsageStats>>,
    known: std::sync::Arc<std::sync::atomic::AtomicBool>,
    prior_known: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for ExecutionUsageState {
    fn default() -> Self {
        Self {
            usage: std::sync::Arc::new(std::sync::Mutex::new(TokenUsageStats::default())),
            known: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            prior_known: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

impl ExecutionUsageState {
    pub(crate) fn reset(&self) {
        *self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = TokenUsageStats::default();
        self.known.store(true, std::sync::atomic::Ordering::Release);
        self.prior_known
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn set(&self, usage: TokenUsageStats, known: bool) {
        *self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = usage;
        self.known
            .store(known, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn begin_provider_call(&self) {
        let prior = self.known.swap(false, std::sync::atomic::Ordering::AcqRel);
        self.prior_known
            .store(prior, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn record_provider_response(&self, usage: &TokenUsageStats) {
        self.merge(usage);
        self.known.store(
            self.prior_known.load(std::sync::atomic::Ordering::Acquire),
            std::sync::atomic::Ordering::Release,
        );
    }

    pub(crate) fn merge(&self, usage: &TokenUsageStats) {
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merge(usage);
    }

    pub(crate) fn mark_unknown(&self) {
        self.known
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn usage_snapshot(&self) -> TokenUsageStats {
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn snapshot(&self) -> Option<TokenUsageStats> {
        self.known
            .load(std::sync::atomic::Ordering::Acquire)
            .then(|| self.usage_snapshot())
    }

    pub(crate) fn measurement(&self) -> axocoatl_core::MeasuredTokenUsage {
        let usage = self.usage_snapshot();
        if self.known.load(std::sync::atomic::Ordering::Acquire) {
            axocoatl_core::MeasuredTokenUsage::known(usage)
        } else {
            axocoatl_core::MeasuredTokenUsage::lower_bound(usage)
        }
    }
}

use crate::error::AgentError;
use crate::run_control::{AgentRunControl, AgentRunOutcome};

/// What the supervisor does when a child fails.
#[derive(Debug)]
pub enum SupervisionDecision {
    Restart,
    Stop,
    Escalate,
}

/// A chunk of an agent's streamed output, forwarded to observers (the daemon
/// stream bus → the app WebSocket) while the agent is generating.
#[derive(Debug, Clone)]
pub enum AgentStreamChunk {
    /// Assistant text token(s).
    Text(String),
    /// Reasoning / "thinking" token(s) — extended-thinking models.
    Reasoning(String),
    /// A tool call is about to run — surfaced so the UI can render a live
    /// tool-call card.
    ToolCallStarted {
        /// Logical child agent that produced this evidence. Standalone agents
        /// emit `None`; coordinators set the worker id while forwarding.
        source_agent: Option<String>,
        id: String,
        name: String,
        /// Arguments actually dispatched after policy hooks transform them.
        arguments: serde_json::Value,
        /// Immutable model-authored arguments retained for provider-native
        /// history replay. Policy transforms must never rewrite model history.
        provider_arguments: serde_json::Value,
        /// Opaque provider-native replay state. It is persisted beside the
        /// call evidence but never exposed as executor arguments.
        provider_metadata: ProviderMetadata,
        /// Universal assistant text emitted alongside this tool-call group.
        /// Present only on call index zero so a large response is not cloned
        /// once per parallel call before bounded ledger persistence.
        assistant_content: Option<String>,
        /// One-based provider tool-loop round within this execution. Parallel
        /// calls from one assistant response share the value; a later model
        /// response uses the next value so durable projection can restore the
        /// original assistant-call grouping.
        provider_response_group: u64,
        /// Zero-based position in the provider's original assistant response.
        /// Hooks may finish denied calls before allowed calls start, so event
        /// delivery order alone cannot reconstruct a parallel native turn.
        provider_call_index: usize,
        /// Total calls in the original provider response. Durable projection
        /// rejects incomplete groups instead of replaying a truncated native
        /// turn after cancellation or bounded event loss.
        provider_call_count: usize,
    },
    /// A tool call finished — carries its result (or error) for the card.
    ToolCallResult {
        /// Logical child agent that produced this evidence. Standalone agents
        /// emit `None`; coordinators set the worker id while forwarding.
        source_agent: Option<String>,
        id: String,
        name: String,
        result: serde_json::Value,
        is_error: bool,
    },
}

/// Where an agent forwards its streamed output. The daemon attaches one of
/// these before a streaming execution; non-streaming callers pass `None`.
pub type StreamSink = tokio::sync::mpsc::UnboundedSender<AgentStreamChunk>;

/// Outcome of a background "sleep-time" consolidation pass.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    /// True when the pass did no work (agent not idle long enough, or no memory).
    pub skipped: bool,
    /// Durable facts promoted into core-memory blocks.
    pub promoted: usize,
    /// Blocks rewritten / tightened / deduped.
    pub rewritten: usize,
    /// Labels of the blocks that were touched.
    pub blocks_touched: Vec<String>,
    /// Tokens the consolidation LLM call spent.
    pub tokens_used: usize,
}

impl ConsolidationReport {
    /// A no-work report (the actor was active too recently, or there is no memory).
    pub fn skipped() -> Self {
        Self {
            skipped: true,
            ..Default::default()
        }
    }
}

/// Every Axocoatl agent implements this trait.
/// The ractor Actor trait is the execution primitive;
/// AgentBehavior is the domain-level interface.
///
/// Uses `#[async_trait]` because behaviors need dynamic dispatch (`Box<dyn AgentBehavior>`).
/// ractor's own Actor trait uses RPITIT on the concrete AgentActor struct — no conflict.
#[async_trait::async_trait]
pub trait AgentBehavior: Send + Sync + 'static {
    /// Called once at startup — initialize any external connections.
    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError>;

    /// Main execution — process a single input, return output.
    /// This is where the LLM call happens.
    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError>;

    /// Return this behavior's authoritative cumulative provider usage when it
    /// owns durable accounting state. The actor wrapper uses the snapshot on
    /// startup and after both successful and failed executions, avoiding a
    /// second merge of usage already included by the behavior. Custom
    /// behaviors that only report per-call usage in `AgentOutput` retain the
    /// actor wrapper's legacy accumulation by returning `None`.
    fn cumulative_token_usage(&self) -> Option<TokenUsageStats> {
        None
    }

    /// Cumulative usage together with sticky completeness. Behaviors that
    /// already expose an authoritative cumulative subtotal but do not need a
    /// custom completeness model are treated as complete by default.
    fn cumulative_token_usage_measurement(&self) -> Option<axocoatl_core::MeasuredTokenUsage> {
        self.cumulative_token_usage()
            .map(axocoatl_core::MeasuredTokenUsage::known)
    }

    /// Exact/estimated usage for the most recent Execute activation. `None`
    /// means a provider call failed or was cancelled without terminal usage,
    /// so treating the execution as zero-cost would be dishonest.
    fn last_execution_token_usage(&self) -> Option<TokenUsageStats> {
        None
    }

    /// Usage for the most recent Execute activation together with its
    /// completeness. This preserves the known subtotal when a later provider
    /// call in the same activation ends without terminal usage.
    fn last_execution_token_usage_measurement(&self) -> Option<axocoatl_core::MeasuredTokenUsage> {
        self.last_execution_token_usage()
            .map(axocoatl_core::MeasuredTokenUsage::known)
    }

    /// Execute with a caller-owned run identity and cooperative cancellation.
    ///
    /// The default preserves compatibility for custom behaviors: it checks the
    /// request before and after their existing `execute` implementation. The
    /// standard behavior overrides this method to cancel provider streaming and
    /// stop its multi-step tool loop at safe boundaries.
    async fn execute_controlled(
        &mut self,
        input: AgentInput,
        control: AgentRunControl,
    ) -> Result<AgentRunOutcome, AgentError> {
        if control.is_cancelled() {
            return Ok(AgentRunOutcome::Cancelled {
                run_id: control.id().clone(),
                partial_output: AgentOutput::text(""),
            });
        }
        let output = self.execute(input).await?;
        if control.is_cancelled() {
            Ok(AgentRunOutcome::Cancelled {
                run_id: control.id().clone(),
                partial_output: output,
            })
        } else {
            Ok(AgentRunOutcome::Completed(output))
        }
    }

    /// Attach (or clear) a sink that receives this agent's streamed output
    /// chunks during the next `execute`. Default: no-op — behaviors that do
    /// not stream simply ignore it.
    fn set_stream_sink(&mut self, _sink: Option<StreamSink>) {}

    /// Called when a supervised child agent fails.
    async fn on_child_failure(
        &mut self,
        _child_id: AgentId,
        _error: AgentError,
    ) -> SupervisionDecision {
        SupervisionDecision::Restart
    }

    /// Background "sleep-time" consolidation — invoked explicitly after the
    /// agent has been idle by the daemon's config-gated consolidation loop.
    /// Generic actor Stop never starts provider work or memory mutation.
    /// Promotes durable facts into curated memory and tidies it. Default: no-op.
    async fn on_consolidate(&mut self) -> Result<ConsolidationReport, AgentError> {
        Ok(ConsolidationReport::default())
    }

    /// Called on graceful shutdown.
    async fn on_stop(&mut self) -> Result<(), AgentError>;
}
