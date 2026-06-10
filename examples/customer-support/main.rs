//! Customer Support Agent — single agent demonstrating 4-tier memory, session resume, and skills.
//!
//! Architecture:
//!   [Customer Support Agent]
//!     ├─ Tier 1: SessionMemory  (in-memory conversation transcript)
//!     ├─ Tier 2: CheckpointStore (persistent state snapshots for crash recovery)
//!     ├─ Skills: SkillRegistry  (reusable prompt templates for common tasks)
//!     └─ LLM: MockProvider      (simulated responses)
//!
//! Demonstrates:
//!   - 4-tier memory system (session + checkpoint tiers shown; semantic/long-term are
//!     optional heavy deps behind feature flags)
//!   - Session resume after simulated crash via `CheckpointStore`
//!   - `SkillRegistry` with custom and built-in skills
//!   - `DefaultAgentBehavior` wired with checkpoint store
//!   - Multi-turn conversation tracking in `SessionMemory`
//!
//! Run: `cargo run` from examples/customer-support/

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use ractor::Actor;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, DefaultAgentBehavior};
use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, ChatMessage, OverflowPolicy, Skill, SkillParameter,
    SkillRegistry, TokenBudget, TokenUsageStats,
};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_memory::{CheckpointPolicy, CheckpointStore};
use axocoatl_token::TokenCounter;

// ---------------------------------------------------------------------------
// Mock LLM Provider — simulates customer support responses
// ---------------------------------------------------------------------------

/// Tracks conversation turn count to vary responses across a multi-turn session.
struct MockSupportLlm {
    turn: std::sync::atomic::AtomicUsize,
}

impl MockSupportLlm {
    fn new() -> Self {
        Self {
            turn: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockSupportLlm {
    fn provider_id(&self) -> &str {
        "mock-support"
    }

    fn model_id(&self) -> &str {
        "mock-support-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            structured_output: true,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 32_000,
            max_output_tokens: 2_048,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let turn = self.turn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let last_msg = request
            .messages
            .last()
            .and_then(|m| m.text_content())
            .unwrap_or("");

        // Vary response based on conversation turn and content
        let content = if last_msg.contains("order status") || last_msg.contains("ORDER-") {
            "I found your order! **Order #ORDER-7829** is currently in transit. \
             It shipped from our warehouse on March 28th and is expected to arrive \
             by April 2nd. The tracking number is TRK-9182736450.\n\n\
             Would you like me to:\n\
             1. Send you a tracking link via email?\n\
             2. Set up delivery notifications?\n\
             3. Help with anything else regarding this order?"
                .to_string()
        } else if last_msg.contains("refund") || last_msg.contains("return") {
            "I understand you'd like to process a return. Our return policy allows \
             returns within 30 days of delivery.\n\n\
             To initiate the return process, I'll need:\n\
             - Your order number\n\
             - Reason for return\n\
             - Whether you'd prefer a refund or exchange\n\n\
             I can also apply our **Satisfaction Guarantee** skill to check if you \
             qualify for expedited processing. Shall I proceed?"
                .to_string()
        } else if last_msg.contains("tracking") || last_msg.contains("delivery") {
            "Your package with tracking number TRK-9182736450 is currently at the \
             regional distribution center. Here's the latest status:\n\n\
             - **Mar 28**: Shipped from warehouse\n\
             - **Mar 29**: Arrived at regional hub\n\
             - **Mar 30**: Out for sorting\n\
             - **ETA**: April 2nd\n\n\
             I've also set up automatic delivery notifications for you. You'll \
             receive an SMS when the package is out for delivery."
                .to_string()
        } else if turn == 0 {
            "Hello! Welcome to Axocoatl Support. I'm your AI customer service agent. \
             I can help you with:\n\n\
             - **Order tracking** and status updates\n\
             - **Returns and refunds** processing\n\
             - **Product information** and recommendations\n\
             - **Account management**\n\n\
             How can I assist you today?"
                .to_string()
        } else {
            format!(
                "Thank you for that information. I've noted your request and I'm \
                 looking into it now. Based on your account history, I can see you've \
                 been a valued customer since 2024.\n\n\
                 Is there anything specific about \"{}\" that I should focus on?",
                &last_msg[..50.min(last_msg.len())]
            )
        };

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(120, 80),
            model: "mock-support-v1".to_string(),
            provider: "mock-support".to_string(),
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
// Skill Setup — register domain-specific skills for customer support
// ---------------------------------------------------------------------------

fn build_skill_registry() -> SkillRegistry {
    let mut registry = SkillRegistry::new();

    // Register built-in skills (summarize, code_review, research, translate)
    registry.register_builtins();

    // Register domain-specific customer support skills
    registry.register(Skill {
        name: "order_lookup".to_string(),
        description: "Look up an order by order number and return its status".to_string(),
        template: "Look up the following order and provide a complete status update \
                   including shipping details, estimated delivery, and any issues:\n\n\
                   Order Number: {{order_number}}\n\
                   Customer Name: {{customer_name}}"
            .to_string(),
        parameters: vec![
            SkillParameter {
                name: "order_number".to_string(),
                description: "The order number to look up".to_string(),
                required: true,
                default: None,
            },
            SkillParameter {
                name: "customer_name".to_string(),
                description: "Customer's name for verification".to_string(),
                required: false,
                default: Some("Unknown".to_string()),
            },
        ],
    });

    registry.register(Skill {
        name: "satisfaction_guarantee".to_string(),
        description: "Check if a customer qualifies for the satisfaction guarantee program"
            .to_string(),
        template: "Evaluate the following customer for our Satisfaction Guarantee program:\n\n\
                   Customer tenure: {{tenure}}\n\
                   Order history: {{order_count}} orders\n\
                   Issue type: {{issue_type}}\n\n\
                   Criteria: Customers with 6+ months tenure and 3+ orders qualify for \
                   expedited returns, free return shipping, and priority refund processing."
            .to_string(),
        parameters: vec![
            SkillParameter {
                name: "tenure".to_string(),
                description: "How long the customer has been active".to_string(),
                required: true,
                default: None,
            },
            SkillParameter {
                name: "order_count".to_string(),
                description: "Number of previous orders".to_string(),
                required: true,
                default: None,
            },
            SkillParameter {
                name: "issue_type".to_string(),
                description: "Type of issue (return, refund, exchange)".to_string(),
                required: true,
                default: None,
            },
        ],
    });

    registry.register(Skill {
        name: "escalation_template".to_string(),
        description: "Generate an escalation ticket for a human agent".to_string(),
        template: "Generate a support escalation ticket:\n\n\
                   **Priority**: {{priority}}\n\
                   **Customer**: {{customer_name}}\n\
                   **Issue Summary**: {{issue_summary}}\n\
                   **Steps Taken**: {{steps_taken}}\n\
                   **Recommended Action**: {{recommendation}}"
            .to_string(),
        parameters: vec![
            SkillParameter {
                name: "priority".to_string(),
                description: "Ticket priority".to_string(),
                required: false,
                default: Some("Medium".to_string()),
            },
            SkillParameter {
                name: "customer_name".to_string(),
                description: "Customer name".to_string(),
                required: true,
                default: None,
            },
            SkillParameter {
                name: "issue_summary".to_string(),
                description: "Brief description of the issue".to_string(),
                required: true,
                default: None,
            },
            SkillParameter {
                name: "steps_taken".to_string(),
                description: "What the AI agent already tried".to_string(),
                required: true,
                default: None,
            },
            SkillParameter {
                name: "recommendation".to_string(),
                description: "Suggested resolution".to_string(),
                required: false,
                default: Some("Review and contact customer".to_string()),
            },
        ],
    });

    registry
}

// ---------------------------------------------------------------------------
// Main — multi-turn conversation with session resume and skills
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .init();

    println!("=== Axocoatl Customer Support Example ===\n");

    // -----------------------------------------------------------------------
    // 1. Set up the skill registry
    // -----------------------------------------------------------------------
    let skill_registry = build_skill_registry();
    println!(
        "Loaded {} skills: {:?}\n",
        skill_registry.count(),
        skill_registry.names()
    );

    // Demonstrate skill rendering
    println!("--- Skill Demo: order_lookup ---");
    let order_skill = skill_registry.get("order_lookup").unwrap();
    let mut params = HashMap::new();
    params.insert("order_number".to_string(), "ORDER-7829".to_string());
    params.insert("customer_name".to_string(), "Alice Johnson".to_string());
    let rendered = order_skill.render(&params)?;
    println!("{rendered}\n");

    // Demonstrate satisfaction guarantee skill
    println!("--- Skill Demo: satisfaction_guarantee ---");
    let sg_skill = skill_registry.get("satisfaction_guarantee").unwrap();
    let mut sg_params = HashMap::new();
    sg_params.insert("tenure".to_string(), "14 months".to_string());
    sg_params.insert("order_count".to_string(), "8".to_string());
    sg_params.insert("issue_type".to_string(), "return".to_string());
    let sg_rendered = sg_skill.render(&sg_params)?;
    println!("{sg_rendered}\n");

    // -----------------------------------------------------------------------
    // 2. Set up checkpoint store (Tier 2 memory — persistent state)
    // -----------------------------------------------------------------------
    let checkpoint_dir = std::env::temp_dir().join("axocoatl-example-customer-support");
    println!("Checkpoint directory: {}\n", checkpoint_dir.display());
    let checkpoint_store = Arc::new(CheckpointStore::new(
        &checkpoint_dir,
        CheckpointPolicy::EveryLlmCall,
    ));

    let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);

    // -----------------------------------------------------------------------
    // 3. Configure the support agent
    // -----------------------------------------------------------------------
    let agent_config = AgentConfig {
        id: AgentId::new("support-agent"),
        name: "Customer Support Agent".to_string(),
        provider: "mock-support".to_string(),
        model: "mock-support-v1".to_string(),
        system_prompt: Some(
            "You are a friendly and professional customer support agent for Axocoatl Corp. \
             You help customers with order tracking, returns, refunds, and general inquiries. \
             Always be empathetic and solution-oriented. If you cannot resolve an issue, \
             offer to escalate to a human agent."
                .to_string(),
        ),
        token_budget: Some(TokenBudget {
            per_call: 2_048,
            per_execution: 8_000,
            // Context compaction toward the model window is automatic; this policy
            // is only the spend cap. Warn = log and keep going past the budget.
            overflow_policy: OverflowPolicy::Warn,
        }),
        tools: vec![
            "order_lookup".to_string(),
            "customer_history".to_string(),
            "create_ticket".to_string(),
        ],
        ..AgentConfig::default()
    };

    // -----------------------------------------------------------------------
    // 4. Phase 1 — Initial conversation session
    // -----------------------------------------------------------------------
    println!("{}", "=".repeat(60));
    println!("PHASE 1: Initial Customer Conversation");
    println!("{}", "=".repeat(60));

    let provider: Arc<dyn LlmProvider> = Arc::new(MockSupportLlm::new());
    let behavior = DefaultAgentBehavior::new(provider, counter.clone())
        .with_checkpoint_store(checkpoint_store.clone());

    let (agent_ref, agent_handle) = AgentActor::spawn(
        Some("support-agent-v1".to_string()),
        AgentActor,
        (
            agent_config.clone(),
            Box::new(behavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    // Turn 1: Greeting
    println!("\n[Customer]: Hi, I need help with my order.");
    let response1 = execute_agent(
        &agent_ref,
        AgentInput::text("Hi, I need help with my order."),
    )
    .await
    .map_err(|e| format!("Agent failed: {e}"))?;
    println!("[Agent]: {}\n", response1.content);
    println!("  (tokens: {} total)", response1.token_usage.total());

    // Turn 2: Order inquiry
    println!("[Customer]: Can you check the status of ORDER-7829?");
    let response2 = execute_agent(
        &agent_ref,
        AgentInput::text("Can you check the order status of ORDER-7829?"),
    )
    .await
    .map_err(|e| format!("Agent failed: {e}"))?;
    println!("[Agent]: {}\n", response2.content);
    println!("  (tokens: {} total)", response2.token_usage.total());

    // Turn 3: Follow-up about tracking
    println!("[Customer]: Yes, please send me the tracking details.");
    let response3 = execute_agent(
        &agent_ref,
        AgentInput::text("Yes, please send me the tracking and delivery details."),
    )
    .await
    .map_err(|e| format!("Agent failed: {e}"))?;
    println!("[Agent]: {}\n", response3.content);
    println!("  (tokens: {} total)", response3.token_usage.total());

    // Report token usage after 3 turns
    let mut phase1_usage = TokenUsageStats::default();
    phase1_usage.merge(&response1.token_usage);
    phase1_usage.merge(&response2.token_usage);
    phase1_usage.merge(&response3.token_usage);

    println!("{}", "─".repeat(60));
    println!(
        "Phase 1 total: {} tokens ({} input + {} output) across 3 turns",
        phase1_usage.total(),
        phase1_usage.input_tokens,
        phase1_usage.output_tokens
    );

    // Simulate crash — stop the agent
    println!("\n*** Simulating agent crash/restart ***");
    agent_ref.stop(None);
    agent_handle.await?;
    println!("Agent stopped. Checkpoint data persisted to disk.\n");

    // -----------------------------------------------------------------------
    // 5. Phase 2 — Resume session from checkpoint
    // -----------------------------------------------------------------------
    println!("{}", "=".repeat(60));
    println!("PHASE 2: Session Resume After Crash");
    println!("{}", "=".repeat(60));

    // Create a fresh provider and behavior — checkpoint will restore conversation
    let provider2: Arc<dyn LlmProvider> = Arc::new(MockSupportLlm::new());
    let behavior2 = DefaultAgentBehavior::new(provider2, counter.clone())
        .with_checkpoint_store(checkpoint_store.clone());

    let (agent_ref2, agent_handle2) = AgentActor::spawn(
        Some("support-agent-v2".to_string()),
        AgentActor,
        (
            agent_config.clone(),
            Box::new(behavior2) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    // The agent restored from checkpoint — session memory is intact.
    // Continue the conversation seamlessly.

    // Turn 4: Follow-up on the same order (agent remembers context from checkpoint)
    println!("\n[Customer]: Actually, I'd like to return this order for a refund.");
    let response4 = execute_agent(
        &agent_ref2,
        AgentInput::text(
            "Actually, I changed my mind. I'd like to process a return and refund for this order.",
        ),
    )
    .await
    .map_err(|e| format!("Agent failed: {e}"))?;
    println!("[Agent]: {}\n", response4.content);
    println!("  (tokens: {} total)", response4.token_usage.total());

    // -----------------------------------------------------------------------
    // 6. Use a skill to check satisfaction guarantee eligibility
    // -----------------------------------------------------------------------
    println!("{}", "─".repeat(60));
    println!("\n--- Using Skill: satisfaction_guarantee ---\n");

    let skill = skill_registry.get("satisfaction_guarantee").unwrap();
    let mut skill_params = HashMap::new();
    skill_params.insert("tenure".to_string(), "14 months".to_string());
    skill_params.insert("order_count".to_string(), "8".to_string());
    skill_params.insert("issue_type".to_string(), "return".to_string());

    let skill_prompt = skill.render(&skill_params)?;
    println!("Skill-generated prompt:\n{skill_prompt}\n");

    // Feed the skill-generated prompt to the agent
    let response5 = execute_agent(&agent_ref2, AgentInput::text(&skill_prompt))
        .await
        .map_err(|e| format!("Agent failed: {e}"))?;
    println!("[Agent (skill-augmented)]: {}\n", response5.content);

    // -----------------------------------------------------------------------
    // 7. Demonstrate the escalation skill
    // -----------------------------------------------------------------------
    println!("--- Using Skill: escalation_template ---\n");

    let escalation_skill = skill_registry.get("escalation_template").unwrap();
    let mut esc_params = HashMap::new();
    esc_params.insert("priority".to_string(), "High".to_string());
    esc_params.insert("customer_name".to_string(), "Alice Johnson".to_string());
    esc_params.insert(
        "issue_summary".to_string(),
        "Customer wants return/refund for ORDER-7829".to_string(),
    );
    esc_params.insert(
        "steps_taken".to_string(),
        "Verified order status, provided tracking, checked satisfaction guarantee eligibility"
            .to_string(),
    );
    esc_params.insert(
        "recommendation".to_string(),
        "Approve expedited return under satisfaction guarantee (14mo tenure, 8 orders)".to_string(),
    );

    let escalation_ticket = escalation_skill.render(&esc_params)?;
    println!("Generated Escalation Ticket:\n{escalation_ticket}\n");

    // -----------------------------------------------------------------------
    // 8. Final summary
    // -----------------------------------------------------------------------
    let mut total_usage = phase1_usage;
    total_usage.merge(&response4.token_usage);
    total_usage.merge(&response5.token_usage);

    println!("{}", "=".repeat(60));
    println!("Session Summary");
    println!("{}", "=".repeat(60));
    println!("  Conversation turns: 5 (3 before crash + 2 after resume)");
    println!(
        "  Total tokens used: {} ({} input + {} output)",
        total_usage.total(),
        total_usage.input_tokens,
        total_usage.output_tokens
    );
    println!("  Checkpoint policy: EveryLlmCall");
    println!("  Session resumed successfully: YES");
    println!("  Skills used: order_lookup, satisfaction_guarantee, escalation_template");
    println!("  Skills available: {:?}", skill_registry.names());

    // -----------------------------------------------------------------------
    // 9. Cleanup
    // -----------------------------------------------------------------------
    println!("\nShutting down...");
    agent_ref2.stop(None);
    agent_handle2.await?;

    // Clean up temp checkpoint directory
    tokio::fs::remove_dir_all(&checkpoint_dir).await.ok();

    println!("\n=== Customer Support Example Complete ===");
    Ok(())
}
