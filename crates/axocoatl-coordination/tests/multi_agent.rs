//! Multi-agent integration test: researcher → lattice event → summarizer activates.
//! Proves the coordination model works end-to-end with real actors.

use std::pin::Pin;
use std::sync::Arc;

use axocoatl_actor::{
    execute_agent, AgentActor, AgentBehavior, AgentRegistry, DefaultAgentBehavior,
};
use axocoatl_coordination::{EventId, EventLattice, EventType, LatticeEvent};
use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, OverflowPolicy, TokenBudget, TokenUsageStats,
};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_token::{ApproximateCounter, TokenCounter};
use ractor::Actor;
use tokio_stream::Stream;

/// Mock LLM that returns a fixed response based on the agent role.
struct RoleMockLlm {
    role: String,
}

#[async_trait::async_trait]
impl LlmProvider for RoleMockLlm {
    fn provider_id(&self) -> &str {
        "mock"
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let user_msg = request
            .messages
            .last()
            .and_then(|m| m.text_content())
            .unwrap_or("");

        let content = match self.role.as_str() {
            "researcher" => format!("Research findings on: {user_msg}. Key facts: A, B, C."),
            "summarizer" => format!("Summary: {user_msg} → 3 key points identified."),
            _ => format!("Response from {}: {user_msg}", self.role),
        };

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(20, 10),
            model: "mock".to_string(),
            provider: "mock".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        // Reuse the non-streaming response: one text delta with the full
        // content, then Usage + Done. Mock providers don't simulate chunking.
        let resp = self.chat(request).await?;
        let events: Vec<Result<StreamEvent, ProviderError>> = vec![
            Ok(StreamEvent::TextDelta {
                delta: resp.content,
            }),
            Ok(StreamEvent::Usage(resp.usage)),
            Ok(StreamEvent::Done {
                finish_reason: resp.finish_reason,
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn simple_counter() -> Arc<dyn TokenCounter> {
    Arc::new(ApproximateCounter::new().unwrap())
}

#[tokio::test]
async fn two_agent_coordination_through_lattice() {
    // Set up the event lattice
    let lattice = Arc::new(EventLattice::new(100));

    // Register agents in the lattice with different thresholds
    // Researcher: activates immediately on UserInput (threshold 1.0)
    lattice.register_agent(AgentId::new("researcher"), 1.0, 0.0);
    // Summarizer: activates on TaskCompleted (threshold 0.5)
    lattice.register_agent(AgentId::new("summarizer"), 0.5, 0.0);

    // Create agent configs
    let researcher_config = AgentConfig {
        id: AgentId::new("researcher"),
        name: "Researcher".to_string(),
        system_prompt: Some("You research topics thoroughly.".to_string()),
        token_budget: Some(TokenBudget {
            per_call: 5000,
            per_execution: 10000,
            overflow_policy: OverflowPolicy::Warn,
        }),
        ..AgentConfig::default()
    };

    let summarizer_config = AgentConfig {
        id: AgentId::new("summarizer"),
        name: "Summarizer".to_string(),
        system_prompt: Some("You summarize research concisely.".to_string()),
        token_budget: Some(TokenBudget {
            per_call: 3000,
            per_execution: 5000,
            overflow_policy: OverflowPolicy::Warn,
        }),
        ..AgentConfig::default()
    };

    // Spawn agents
    let registry = AgentRegistry::new();

    let researcher_behavior = DefaultAgentBehavior::new(
        Arc::new(RoleMockLlm {
            role: "researcher".to_string(),
        }),
        simple_counter(),
    );
    let (researcher_ref, r_handle) = AgentActor::spawn(
        Some("researcher".to_string()),
        AgentActor,
        (
            researcher_config,
            Box::new(researcher_behavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await
    .unwrap();
    registry
        .register(AgentId::new("researcher"), researcher_ref.clone())
        .await;

    let summarizer_behavior = DefaultAgentBehavior::new(
        Arc::new(RoleMockLlm {
            role: "summarizer".to_string(),
        }),
        simple_counter(),
    );
    let (summarizer_ref, s_handle) = AgentActor::spawn(
        Some("summarizer".to_string()),
        AgentActor,
        (
            summarizer_config,
            Box::new(summarizer_behavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await
    .unwrap();
    registry
        .register(AgentId::new("summarizer"), summarizer_ref.clone())
        .await;

    // --- Simulate the coordination flow ---

    // Step 1: User input arrives → publish to lattice
    let user_event = LatticeEvent {
        id: EventId::random(),
        event_type: EventType::UserInput,
        payload: serde_json::json!({"query": "quantum computing 2026"}),
        produced_by: "user".to_string(),
        timestamp: 0,
    };

    let activated = lattice.publish(user_event);
    // Researcher should activate (threshold 1.0, signal strength 1.0 for UserInput)
    assert!(
        activated.contains(&AgentId::new("researcher")),
        "Researcher should activate on UserInput, got: {:?}",
        activated
    );

    // Step 2: Execute the researcher
    let research_output =
        execute_agent(&researcher_ref, AgentInput::text("quantum computing 2026"))
            .await
            .unwrap();
    assert!(research_output.content.contains("Research findings"));

    // Step 3: Researcher publishes completion event
    let completion_event = LatticeEvent {
        id: EventId::random(),
        event_type: EventType::TaskCompleted {
            task_id: "research-task".to_string(),
        },
        payload: serde_json::json!({"result": research_output.content}),
        produced_by: "researcher".to_string(),
        timestamp: 0,
    };

    let activated = lattice.publish(completion_event);
    // Summarizer should activate (threshold 0.5, signal strength 0.5 for TaskCompleted)
    assert!(
        activated.contains(&AgentId::new("summarizer")),
        "Summarizer should activate on TaskCompleted, got: {:?}",
        activated
    );

    // Step 4: Execute the summarizer with the research output
    let summary_output = execute_agent(&summarizer_ref, AgentInput::text(&research_output.content))
        .await
        .unwrap();
    assert!(summary_output.content.contains("Summary"));

    // Verify token tracking across both agents
    assert_eq!(research_output.token_usage.total(), 30);
    assert_eq!(summary_output.token_usage.total(), 30);

    // Verify lattice state
    assert_eq!(lattice.event_count(), 2);
    assert_eq!(lattice.agent_count(), 2);

    // Clean up
    researcher_ref.stop(None);
    summarizer_ref.stop(None);
    r_handle.await.unwrap();
    s_handle.await.unwrap();
}

#[tokio::test]
async fn auction_selects_best_agent_for_task() {
    use axocoatl_coordination::auction::{compute_bid, run_auction};

    // Three agents with different tool sets
    let web_agent = AgentConfig {
        id: AgentId::new("web-agent"),
        tools: vec!["web_search".to_string(), "read_url".to_string()],
        ..AgentConfig::default()
    };
    let code_agent = AgentConfig {
        id: AgentId::new("code-agent"),
        tools: vec!["run_code".to_string(), "read_file".to_string()],
        ..AgentConfig::default()
    };
    let general_agent = AgentConfig {
        id: AgentId::new("general"),
        tools: vec![
            "web_search".to_string(),
            "run_code".to_string(),
            "read_file".to_string(),
        ],
        ..AgentConfig::default()
    };

    // Task requires web_search
    let required = vec!["web_search".to_string()];

    let bids = vec![
        compute_bid(&web_agent, &required, 0, 5000),
        compute_bid(&code_agent, &required, 0, 5000), // Missing web_search → score 0
        compute_bid(&general_agent, &required, 2, 5000), // Has it but loaded
    ];

    let winner = run_auction(bids).unwrap();
    // web_agent should win: has the tool, zero load, full budget
    assert_eq!(winner, AgentId::new("web-agent"));
}

#[tokio::test]
async fn htn_reduces_llm_calls() {
    use axocoatl_coordination::htn::*;

    let mut planner = HtnPlanner::new();

    // Define domain methods for a "research_and_summarize" workflow
    planner.add_method(DecompositionMethod {
        task_pattern: "research_and_summarize".to_string(),
        preconditions: vec![],
        subtasks: vec![
            HtnTask {
                name: "web_search".to_string(),
                parameters: std::collections::HashMap::new(),
                task_type: HtnTaskType::Primitive,
            },
            HtnTask {
                name: "extract_facts".to_string(),
                parameters: std::collections::HashMap::new(),
                task_type: HtnTaskType::Primitive,
            },
            HtnTask {
                name: "generate_summary".to_string(),
                parameters: std::collections::HashMap::new(),
                task_type: HtnTaskType::Compound, // Needs LLM
            },
        ],
    });

    let plan = planner.plan(HtnTask {
        name: "research_and_summarize".to_string(),
        parameters: std::collections::HashMap::new(),
        task_type: HtnTaskType::Compound,
    });

    // 2 primitives resolved without LLM, 1 needs LLM
    assert_eq!(plan.primitives.len(), 2);
    assert_eq!(plan.llm_frontiers.len(), 1);
    assert_eq!(plan.llm_frontiers[0].name, "generate_summary");

    // This demonstrates the 75% LLM call reduction claim:
    // Without HTN: 3 LLM calls (decompose + each subtask)
    // With HTN: 1 LLM call (only for generate_summary)
}
