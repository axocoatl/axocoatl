//! Code Reviewer — 3-agent system demonstrating coordinator mode with tools.
//!
//! Architecture:
//!   User submits code -> [Coordinator] decomposes into subtasks ->
//!     [Reader Agent]   reads and parses the code structure
//!     [Analyzer Agent]  checks for bugs, complexity, and style issues
//!     [Reporter Agent]  formats the combined findings into a review report
//!   <- Coordinator synthesizes final review
//!
//! Demonstrates:
//!   - `CoordinatorBehavior` orchestrating worker agents
//!   - `WorkerConfig` for defining worker capabilities
//!   - `DefaultAgentBehavior` with mock LLM providers
//!   - Automatic task decomposition and delegation
//!   - Token usage tracking across coordinator + workers
//!
//! Run: `cargo run` from examples/code-reviewer/

use std::pin::Pin;
use std::sync::Arc;

use ractor::Actor;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, CoordinatorBehavior, WorkerConfig};
use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, ChatMessage, OverflowPolicy, TokenBudget, TokenUsageStats,
};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_token::TokenCounter;

// ---------------------------------------------------------------------------
// Mock LLM Providers — simulate different agent capabilities
// ---------------------------------------------------------------------------

/// Mock LLM that simulates the coordinator's task decomposition and synthesis.
/// The coordinator calls the LLM twice:
///   1. To decompose the task into subtasks (returns JSON)
///   2. To synthesize worker results into a final response
struct MockCoordinatorLlm;

#[async_trait::async_trait]
impl LlmProvider for MockCoordinatorLlm {
    fn provider_id(&self) -> &str {
        "mock-coordinator"
    }

    fn model_id(&self) -> &str {
        "mock-coordinator-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            structured_output: true,
            vision: false,
            reasoning: true,
            embeddings: false,
            max_context_tokens: 200_000,
            max_output_tokens: 8_192,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let last_msg = request
            .messages
            .last()
            .and_then(|m| m.text_content())
            .unwrap_or("");

        // Detect whether this is a decomposition call or a synthesis call
        let content = if last_msg.contains("Break the following task") {
            // Task decomposition: return JSON array of subtasks
            serde_json::json!([
                {
                    "name": "code_structure_analysis",
                    "description": "Read and parse the code structure. Identify functions, types, imports, and module organization. Report the code architecture."
                },
                {
                    "name": "bug_and_complexity_analysis",
                    "description": "Analyze the code for potential bugs, logic errors, unsafe patterns, and cyclomatic complexity. Flag any issues with severity levels."
                },
                {
                    "name": "style_and_report",
                    "description": "Check code style consistency, naming conventions, documentation coverage, and format all findings into a structured review report."
                }
            ])
            .to_string()
        } else {
            // Synthesis: combine worker results into final review
            "# Code Review Report\n\n\
             ## Overall Assessment: APPROVED with suggestions\n\n\
             The code is well-structured with clear separation of concerns. \
             The module organization follows Rust best practices.\n\n\
             ## Findings\n\n\
             ### Structure (from Reader)\n\
             - 3 modules, 7 functions, 2 public types\n\
             - Clear dependency graph with no circular imports\n\n\
             ### Issues (from Analyzer)\n\
             - **Medium**: Potential panic on unwrap() at line 42 — use `?` operator instead\n\
             - **Low**: Clone on large struct could be replaced with Arc for shared ownership\n\
             - Cyclomatic complexity: 4.2 average (good)\n\n\
             ### Style (from Reporter)\n\
             - Documentation coverage: 85% (above threshold)\n\
             - Naming conventions: consistent snake_case\n\
             - Suggestion: Add `#[must_use]` to builder methods\n\n\
             ## Recommendation\n\
             Merge after addressing the Medium-severity unwrap issue."
                .to_string()
        };

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(300, 200),
            model: "mock-coordinator-v1".to_string(),
            provider: "mock-coordinator".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "Streaming not supported in mock".to_string(),
        ))
    }
}

/// Mock LLM for worker agents — returns role-appropriate analysis.
/// Kept as a reference implementation of `LlmProvider` for the example;
/// the coordinator path constructs its workers differently.
#[allow(dead_code)]
struct MockWorkerLlm {
    role: String,
}

#[allow(dead_code)]
impl MockWorkerLlm {
    fn new(role: &str) -> Self {
        Self {
            role: role.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockWorkerLlm {
    fn provider_id(&self) -> &str {
        "mock-worker"
    }

    fn model_id(&self) -> &str {
        "mock-worker-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 32_000,
            max_output_tokens: 2_048,
        }
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let content = match self.role.as_str() {
            "reader" => "Code Structure Analysis:\n\
                 - Module: `lib.rs` (public API surface)\n\
                 - Module: `parser.rs` (internal parsing logic)\n\
                 - Module: `types.rs` (shared data types)\n\
                 - Functions: 7 total (4 public, 3 private)\n\
                 - Types: `Config` (pub struct), `ParseResult` (pub enum)\n\
                 - Imports: serde, tokio, thiserror\n\
                 - Architecture: layered with clear boundaries"
                .to_string(),
            "analyzer" => "Bug & Complexity Analysis:\n\
                 - [MEDIUM] Line 42: `unwrap()` on user input — could panic on malformed data\n\
                 - [LOW] Line 78: `clone()` on `Config` struct (128 bytes) in hot loop\n\
                 - [INFO] Line 15: Consider `#[non_exhaustive]` on `ParseResult` enum\n\
                 - Cyclomatic complexity: avg 4.2, max 8 (within acceptable range)\n\
                 - No unsafe code detected\n\
                 - No data races possible (all types are Send + Sync)"
                .to_string(),
            "reporter" => "Style & Documentation Report:\n\
                 - Documentation coverage: 85% (6/7 public items documented)\n\
                 - Missing: `parse_with_options()` needs doc comment\n\
                 - Naming: consistent snake_case throughout\n\
                 - Formatting: rustfmt-compliant\n\
                 - Suggestions:\n\
                   * Add `#[must_use]` to `ConfigBuilder` methods\n\
                   * Consider `impl Display` for `ParseResult`\n\
                   * Add module-level documentation to `parser.rs`"
                .to_string(),
            _ => format!("Worker '{}' completed analysis.", self.role),
        };

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(150, 100),
            model: "mock-worker-v1".to_string(),
            provider: "mock-worker".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "Streaming not supported in mock".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Simple Token Counter
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
// Main — demonstrate the coordinator pattern
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .init();

    println!("=== Axocoatl Code Reviewer Example ===\n");

    let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
    let coordinator_provider: Arc<dyn LlmProvider> = Arc::new(MockCoordinatorLlm);

    // -----------------------------------------------------------------------
    // 1. Build the CoordinatorBehavior with worker configurations
    // -----------------------------------------------------------------------
    //
    // The coordinator automatically:
    //   - Decomposes the task using its LLM
    //   - Spawns worker agents with DefaultAgentBehavior
    //   - Delegates subtasks to workers
    //   - Collects results and synthesizes a final response

    let coordinator_behavior = CoordinatorBehavior::new(coordinator_provider, counter.clone())
        .add_worker_config(WorkerConfig {
            id: AgentId::new("reader-worker"),
            name: "Code Reader".to_string(),
            system_prompt: "You are a code structure analyzer. Read code and identify \
                            functions, types, imports, and module organization."
                .to_string(),
            tools: vec!["read_file".to_string(), "list_directory".to_string()],
            token_budget: 50_000,
            recall: axocoatl_core::RecallConfig::default(),
        })
        .add_worker_config(WorkerConfig {
            id: AgentId::new("analyzer-worker"),
            name: "Code Analyzer".to_string(),
            system_prompt: "You are a code quality analyzer. Check for bugs, unsafe patterns, \
                            complexity issues, and potential runtime errors."
                .to_string(),
            tools: vec!["ast_parse".to_string(), "complexity_check".to_string()],
            token_budget: 50_000,
            recall: axocoatl_core::RecallConfig::default(),
        })
        .add_worker_config(WorkerConfig {
            id: AgentId::new("reporter-worker"),
            name: "Report Writer".to_string(),
            system_prompt: "You check code style, documentation coverage, and naming conventions. \
                            Format all findings into a clear review report."
                .to_string(),
            tools: vec!["lint_check".to_string()],
            token_budget: 50_000,
            recall: axocoatl_core::RecallConfig::default(),
        });

    // -----------------------------------------------------------------------
    // 2. Configure the coordinator agent
    // -----------------------------------------------------------------------
    let coordinator_config = AgentConfig {
        id: AgentId::new("code-review-coordinator"),
        name: "Code Review Coordinator".to_string(),
        provider: "mock-coordinator".to_string(),
        model: "mock-coordinator-v1".to_string(),
        system_prompt: Some(
            "You are a senior code reviewer coordinating a team of specialized \
             analysis agents. Decompose review tasks, delegate to workers, and \
             synthesize their findings into a comprehensive review."
                .to_string(),
        ),
        token_budget: Some(TokenBudget {
            per_call: 8_192,
            per_execution: 50_000,
            overflow_policy: OverflowPolicy::Warn,
        }),
        tools: vec![],
        ..AgentConfig::default()
    };

    // -----------------------------------------------------------------------
    // 3. Spawn the coordinator as a ractor actor
    // -----------------------------------------------------------------------
    println!("Spawning code review coordinator with 3 worker slots...\n");

    let (coordinator_ref, coordinator_handle) = AgentActor::spawn(
        Some("code-review-coordinator".to_string()),
        AgentActor,
        (
            coordinator_config,
            Box::new(coordinator_behavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    // -----------------------------------------------------------------------
    // 4. Submit code for review
    // -----------------------------------------------------------------------
    let code_to_review = r#"
// File: src/parser.rs

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub max_retries: u32,
    pub timeout_ms: u64,
    pub features: HashMap<String, bool>,
    pub metadata: Vec<u8>,  // 128+ bytes when populated
}

#[derive(Debug)]
pub enum ParseResult {
    Success(Config),
    Partial { config: Config, warnings: Vec<String> },
    Failed(String),
}

pub fn parse_config(input: &str) -> ParseResult {
    let config: Config = serde_json::from_str(input).unwrap(); // Line 42
    ParseResult::Success(config)
}

pub fn parse_with_options(input: &str, strict: bool) -> ParseResult {
    match serde_json::from_str::<Config>(input) {
        Ok(config) => {
            if strict && config.name.is_empty() {
                ParseResult::Failed("name is required in strict mode".to_string())
            } else {
                ParseResult::Success(config)
            }
        }
        Err(e) => ParseResult::Failed(e.to_string()),
    }
}

fn validate_features(config: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for (key, _) in &config.features {
        if key.contains(' ') {
            warnings.push(format!("Feature key '{}' contains spaces", key));
        }
    }
    warnings
}

pub fn batch_parse(inputs: &[&str]) -> Vec<ParseResult> {
    inputs.iter().map(|input| {
        let config = parse_config(input);
        let cfg_clone = match &config {  // Line 78: clone in loop
            ParseResult::Success(c) => c.clone(),
            _ => return config,
        };
        let warnings = validate_features(&cfg_clone);
        if warnings.is_empty() {
            config
        } else {
            ParseResult::Partial { config: cfg_clone, warnings }
        }
    }).collect()
}
"#;

    println!("Submitting code for review...\n");
    println!("{}", "─".repeat(60));

    let review_input = AgentInput::text(format!(
        "Review the following Rust code for bugs, style issues, and improvements:\n\n{code_to_review}"
    ));

    let result = execute_agent(&coordinator_ref, review_input)
        .await
        .map_err(|e| format!("Code review failed: {e}"))?;

    // -----------------------------------------------------------------------
    // 5. Display the review results
    // -----------------------------------------------------------------------
    println!("\n{}", result.content);

    println!("\n{}", "─".repeat(60));
    println!("\nToken Usage Summary:");
    println!(
        "  Total: {} tokens ({} input + {} output)",
        result.token_usage.total(),
        result.token_usage.input_tokens,
        result.token_usage.output_tokens
    );

    // -----------------------------------------------------------------------
    // 6. Shutdown
    // -----------------------------------------------------------------------
    println!("\nShutting down coordinator...");
    coordinator_ref.stop(None);
    coordinator_handle.await?;

    println!("\n=== Code Review Complete ===");
    Ok(())
}
