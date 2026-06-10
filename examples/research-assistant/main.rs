//! Research Assistant — 2-agent system demonstrating coordination, token budgets, and MCP tools.
//!
//! Architecture:
//!   User query -> [Researcher Agent] -> (raw findings) -> [Summarizer Agent] -> final summary
//!
//! Demonstrates:
//!   - Spawning agents as ractor actors via `AgentActor`
//!   - Custom `AgentBehavior` implementations with mock LLM providers
//!   - Token budget enforcement with `TokenBudget` and `OverflowPolicy`
//!   - Session memory tracking across agent turns
//!   - Agent-to-agent coordination via message passing
//!
//! Run: `cargo run` from examples/research-assistant/

use std::pin::Pin;
use std::sync::Arc;

use ractor::Actor;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError};
use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, AgentOutput, ChatMessage, OverflowPolicy, TokenBudget,
    TokenUsageStats,
};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_memory::SessionMemory;
use axocoatl_token::TokenCounter;

// ---------------------------------------------------------------------------
// Mock LLM Provider — simulates research and summarization responses
// ---------------------------------------------------------------------------

/// A mock LLM that returns different responses based on the system prompt context.
/// In a real application, this would be replaced with an actual provider like
/// `axocoatl_llm_anthropic::AnthropicProvider` or `axocoatl_llm_openai::OpenAiProvider`.
struct MockResearchLlm;

#[async_trait::async_trait]
impl LlmProvider for MockResearchLlm {
    fn provider_id(&self) -> &str {
        "mock-research"
    }

    fn model_id(&self) -> &str {
        "mock-research-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 128_000,
            max_output_tokens: 4_096,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // Extract the user's query from the last message
        let user_query = request
            .messages
            .iter()
            .rev()
            .find(|m| m.text_content().is_some())
            .and_then(|m| m.text_content())
            .unwrap_or("unknown topic");

        // Simulate a research response with mock findings
        let content = format!(
            "## Research Findings: {user_query}\n\n\
            ### Source 1: Academic Paper (2025)\n\
            Recent advances in this area show significant progress. Key findings include \
            improved efficiency metrics (up 35% year-over-year) and novel architectural \
            approaches that reduce computational overhead.\n\n\
            ### Source 2: Industry Report\n\
            Market analysis indicates growing adoption across enterprise sectors. \
            The total addressable market is projected to reach $4.2B by 2027.\n\n\
            ### Source 3: Open-Source Community\n\
            The open-source ecosystem has contributed 12 major frameworks in the past \
            year, with community-driven benchmarks establishing new performance baselines.\n\n\
            ### Key Data Points\n\
            - Performance improvement: 35% YoY\n\
            - Market projection: $4.2B by 2027\n\
            - Active frameworks: 12+\n\
            - Research papers published: 847 in 2025"
        );

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(250, 180),
            model: "mock-research-v1".to_string(),
            provider: "mock-research".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "Streaming not supported in mock provider".to_string(),
        ))
    }
}

/// Mock LLM for the summarizer agent.
struct MockSummarizerLlm;

#[async_trait::async_trait]
impl LlmProvider for MockSummarizerLlm {
    fn provider_id(&self) -> &str {
        "mock-summarizer"
    }

    fn model_id(&self) -> &str {
        "mock-summarizer-v1"
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
        let content = "\
            **Executive Summary**\n\n\
            The research landscape shows strong momentum with 35% year-over-year \
            performance gains driven by novel architectural approaches. Industry adoption \
            is accelerating, with the market projected to reach $4.2B by 2027. The \
            open-source ecosystem remains vibrant with 12+ active frameworks and 847 \
            research papers published this year, establishing new community benchmarks.\n\n\
            **Key Takeaways:**\n\
            1. Performance is improving rapidly (35% YoY)\n\
            2. Enterprise adoption is the primary growth driver\n\
            3. Strong open-source foundation supports continued innovation"
            .to_string();

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(180, 95),
            model: "mock-summarizer-v1".to_string(),
            provider: "mock-summarizer".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "Streaming not supported in mock provider".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Simple Token Counter (for examples — real apps use TiktokenCounter)
// ---------------------------------------------------------------------------

struct SimpleCounter;

impl TokenCounter for SimpleCounter {
    fn count_text(&self, text: &str) -> usize {
        // Rough approximation: ~4 chars per token
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
// Researcher Behavior — calls mock LLM, simulates MCP web_search tool usage
// ---------------------------------------------------------------------------

struct ResearcherBehavior {
    provider: Arc<dyn LlmProvider>,
    counter: Arc<dyn TokenCounter>,
    session: SessionMemory,
}

impl ResearcherBehavior {
    fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            provider,
            counter,
            session: SessionMemory::new(),
        }
    }
}

#[async_trait::async_trait]
impl AgentBehavior for ResearcherBehavior {
    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError> {
        tracing::info!(
            agent = %config.id,
            tools = ?config.tools,
            "Researcher agent initialized with {} MCP tools",
            config.tools.len()
        );
        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        // Track the incoming query in session memory
        let input_tokens = self.counter.count_text(&input.content);
        self.session.append(
            axocoatl_core::MessageRole::User,
            &input.content,
            input_tokens,
        );

        tracing::info!(
            query = %input.content,
            session_tokens = self.session.total_tokens(),
            "Researcher processing query"
        );

        // Simulate MCP tool call: web_search
        let tool_call = axocoatl_core::ToolCallRecord {
            tool_name: "web_search".to_string(),
            arguments: serde_json::json!({
                "query": input.content,
                "max_results": 5
            }),
            result: Some(serde_json::json!({
                "results": [
                    {"title": "Academic Paper 2025", "url": "https://arxiv.org/abs/2025.xxxxx"},
                    {"title": "Industry Report Q1", "url": "https://reports.example.com/q1"},
                    {"title": "OSS Framework Benchmarks", "url": "https://github.com/benchmarks"}
                ]
            })),
        };

        // Build and send the LLM request
        let request = ChatRequest::with_system(
            "You are a thorough research agent. Analyze all available sources and \
             provide detailed findings with citations. Use the web_search tool to \
             gather information.",
            &input.content,
        );

        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;

        // Track the response in session memory
        let output_tokens = self.counter.count_text(&response.content);
        self.session.append(
            axocoatl_core::MessageRole::Assistant,
            &response.content,
            output_tokens,
        );

        tracing::info!(
            session_messages = self.session.len(),
            session_tokens = self.session.total_tokens(),
            usage_input = response.usage.input_tokens,
            usage_output = response.usage.output_tokens,
            "Researcher completed query"
        );

        Ok(AgentOutput {
            content: response.content,
            tool_calls: vec![tool_call],
            token_usage: response.usage,
        })
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        tracing::info!(
            total_messages = self.session.len(),
            total_tokens = self.session.total_tokens(),
            "Researcher agent shutting down"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Summarizer Behavior — takes research output and produces a concise summary
// ---------------------------------------------------------------------------

struct SummarizerBehavior {
    provider: Arc<dyn LlmProvider>,
    counter: Arc<dyn TokenCounter>,
    session: SessionMemory,
}

impl SummarizerBehavior {
    fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            provider,
            counter,
            session: SessionMemory::new(),
        }
    }
}

#[async_trait::async_trait]
impl AgentBehavior for SummarizerBehavior {
    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError> {
        tracing::info!(agent = %config.id, "Summarizer agent initialized");
        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        let input_tokens = self.counter.count_text(&input.content);
        self.session.append(
            axocoatl_core::MessageRole::User,
            &input.content,
            input_tokens,
        );

        tracing::info!(
            input_length = input.content.len(),
            "Summarizer condensing research findings"
        );

        let request = ChatRequest::with_system(
            "You are a concise summarizer. Take detailed research findings and produce \
             a brief executive summary with key takeaways. Keep it under 200 words.",
            &input.content,
        );

        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;

        let output_tokens = self.counter.count_text(&response.content);
        self.session.append(
            axocoatl_core::MessageRole::Assistant,
            &response.content,
            output_tokens,
        );

        tracing::info!(
            summary_length = response.content.len(),
            usage_total = response.usage.total(),
            "Summarizer produced summary"
        );

        Ok(AgentOutput {
            content: response.content,
            tool_calls: vec![],
            token_usage: response.usage,
        })
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        tracing::info!("Summarizer agent shutting down");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Main — orchestrate the 2-agent research pipeline
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for observability
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .init();

    println!("=== Axocoatl Research Assistant Example ===\n");

    let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);

    // -----------------------------------------------------------------------
    // 1. Configure the Researcher agent with token budget and MCP tools
    // -----------------------------------------------------------------------
    let researcher_config = AgentConfig {
        id: AgentId::new("researcher"),
        name: "Research Agent".to_string(),
        provider: "mock-research".to_string(),
        model: "mock-research-v1".to_string(),
        system_prompt: Some(
            "You are a thorough research agent. Analyze all available sources \
             and provide detailed findings with citations."
                .to_string(),
        ),
        token_budget: Some(TokenBudget {
            per_call: 4_096,
            per_execution: 10_000,
            // Context compaction toward the model window is automatic; this policy
            // is only the spend cap. Warn = log and keep going past the budget.
            overflow_policy: OverflowPolicy::Warn,
        }),
        tools: vec![
            "web_search".to_string(),
            "read_document".to_string(),
            "extract_citations".to_string(),
        ],
        ..AgentConfig::default()
    };

    // -----------------------------------------------------------------------
    // 2. Configure the Summarizer agent with a tighter token budget
    // -----------------------------------------------------------------------
    let summarizer_config = AgentConfig {
        id: AgentId::new("summarizer"),
        name: "Summarizer Agent".to_string(),
        provider: "mock-summarizer".to_string(),
        model: "mock-summarizer-v1".to_string(),
        system_prompt: Some(
            "You are a concise summarizer. Produce brief executive summaries \
             with key takeaways."
                .to_string(),
        ),
        token_budget: Some(TokenBudget {
            per_call: 2_048,
            per_execution: 5_000,
            overflow_policy: OverflowPolicy::Abort,
        }),
        tools: vec![],
        ..AgentConfig::default()
    };

    // -----------------------------------------------------------------------
    // 3. Spawn both agents as ractor actors
    // -----------------------------------------------------------------------
    let research_provider: Arc<dyn LlmProvider> = Arc::new(MockResearchLlm);
    let summarizer_provider: Arc<dyn LlmProvider> = Arc::new(MockSummarizerLlm);

    let researcher_behavior = ResearcherBehavior::new(research_provider, counter.clone());
    let summarizer_behavior = SummarizerBehavior::new(summarizer_provider, counter.clone());

    println!("Spawning researcher agent...");
    let (researcher_ref, researcher_handle) = AgentActor::spawn(
        Some("researcher".to_string()),
        AgentActor,
        (
            researcher_config,
            Box::new(researcher_behavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    println!("Spawning summarizer agent...");
    let (summarizer_ref, summarizer_handle) = AgentActor::spawn(
        Some("summarizer".to_string()),
        AgentActor,
        (
            summarizer_config,
            Box::new(summarizer_behavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    // -----------------------------------------------------------------------
    // 4. Execute the research pipeline
    // -----------------------------------------------------------------------
    let query = "What are the latest advances in agentic AI frameworks?";
    println!("\nUser Query: {query}\n");
    println!("{}", "─".repeat(60));

    // Step A: Researcher gathers raw findings
    println!("\n[Phase 1] Researcher gathering findings...\n");
    let research_output = execute_agent(&researcher_ref, AgentInput::text(query))
        .await
        .map_err(|e| format!("Researcher failed: {e}"))?;

    println!(
        "Researcher findings ({} chars):",
        research_output.content.len()
    );
    println!(
        "{}",
        &research_output.content[..200.min(research_output.content.len())]
    );
    println!("...\n");
    println!(
        "Tools used: {:?}",
        research_output
            .tool_calls
            .iter()
            .map(|tc| &tc.tool_name)
            .collect::<Vec<_>>()
    );
    println!(
        "Tokens: {} input, {} output, {} total",
        research_output.token_usage.input_tokens,
        research_output.token_usage.output_tokens,
        research_output.token_usage.total()
    );

    println!("\n{}", "─".repeat(60));

    // Step B: Summarizer condenses the research into an executive summary
    println!("\n[Phase 2] Summarizer condensing findings...\n");
    let summary_output = execute_agent(&summarizer_ref, AgentInput::text(&research_output.content))
        .await
        .map_err(|e| format!("Summarizer failed: {e}"))?;

    println!("Final Summary:\n");
    println!("{}", summary_output.content);
    println!(
        "\nTokens: {} input, {} output, {} total",
        summary_output.token_usage.input_tokens,
        summary_output.token_usage.output_tokens,
        summary_output.token_usage.total()
    );

    // -----------------------------------------------------------------------
    // 5. Report cumulative token usage across the pipeline
    // -----------------------------------------------------------------------
    let mut total_usage = TokenUsageStats::default();
    total_usage.merge(&research_output.token_usage);
    total_usage.merge(&summary_output.token_usage);

    println!("\n{}", "─".repeat(60));
    println!("\nPipeline Token Summary:");
    println!(
        "  Researcher: {} tokens",
        research_output.token_usage.total()
    );
    println!(
        "  Summarizer: {} tokens",
        summary_output.token_usage.total()
    );
    println!(
        "  Total:      {} tokens ({} input + {} output)",
        total_usage.total(),
        total_usage.input_tokens,
        total_usage.output_tokens
    );

    // -----------------------------------------------------------------------
    // 6. Graceful shutdown
    // -----------------------------------------------------------------------
    println!("\nShutting down agents...");
    researcher_ref.stop(None);
    summarizer_ref.stop(None);
    researcher_handle.await?;
    summarizer_handle.await?;

    println!("\n=== Research Assistant Complete ===");
    Ok(())
}
