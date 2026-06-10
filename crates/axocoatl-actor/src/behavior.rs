use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput};

use crate::error::AgentError;

/// What the supervisor does when a child fails.
#[derive(Debug)]
pub enum SupervisionDecision {
    Restart,
    Stop,
    Escalate,
}

/// A chunk of an agent's streamed output, forwarded to observers (the daemon
/// stream bus → the dashboard WebSocket) while the agent is generating.
#[derive(Debug, Clone)]
pub enum AgentStreamChunk {
    /// Assistant text token(s).
    Text(String),
    /// Reasoning / "thinking" token(s) — extended-thinking models.
    Reasoning(String),
    /// A tool call is about to run — surfaced so the UI can render a live
    /// tool-call card.
    ToolCallStarted {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// A tool call finished — carries its result (or error) for the card.
    ToolCallResult {
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

    /// Background "sleep-time" consolidation — invoked when the agent has been
    /// idle, by the daemon's consolidation loop (and once on graceful stop).
    /// Promotes durable facts into curated memory and tidies it. Default: no-op.
    async fn on_consolidate(&mut self) -> Result<ConsolidationReport, AgentError> {
        Ok(ConsolidationReport::default())
    }

    /// Called on graceful shutdown.
    async fn on_stop(&mut self) -> Result<(), AgentError>;
}
