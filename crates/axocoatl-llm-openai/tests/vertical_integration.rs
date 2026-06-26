//! Vertical integration test: YAML config → parse → provider → mock LLM call → verify.
//!
//! This validates the end-to-end pipeline from Phase 1:
//! axocoatl-config parses YAML → axocoatl-llm-openai creates provider → ChatRequest → ChatResponse

use std::path::PathBuf;
use std::sync::Arc;

use axocoatl_config::parse_config;
use axocoatl_llm::{ChatRequest, ProviderRegistry};
use axocoatl_llm_openai::OpenAiProvider;

const TEST_CONFIG: &str = r#"
agents:
  - id: researcher
    name: "Research Agent"
    provider: openai
    model: gpt-4o
    system_prompt: "You are a research assistant."
    tools:
      - web_search
    token_budget:
      per_execution: 20000
      per_call: 8192
      overflow_policy: abort

  - id: summarizer
    name: "Summary Agent"
    provider: openai
    model: gpt-4o-mini
    system_prompt: "Summarize the research."

providers:
  openai:
    api_key: "sk-test-fake-key"
"#;

#[test]
fn config_parses_and_converts_to_core_types() {
    let config = parse_config(TEST_CONFIG, &PathBuf::from("test.yaml")).unwrap();

    assert_eq!(config.agents.len(), 2);

    // Convert to core types
    let core_agents: Vec<_> = config.agents.iter().map(|a| a.to_core()).collect();

    assert_eq!(core_agents[0].id.0, "researcher");
    assert_eq!(core_agents[0].provider, "openai");
    assert_eq!(core_agents[0].model, "gpt-4o");
    assert!(core_agents[0].system_prompt.is_some());
    assert!(core_agents[0].token_budget.is_some());

    let budget = core_agents[0].token_budget.as_ref().unwrap();
    assert_eq!(budget.per_execution, 20000);
    assert_eq!(budget.per_call, 8192);
    assert!(matches!(
        budget.overflow_policy,
        axocoatl_core::OverflowPolicy::Abort
    ));

    assert_eq!(core_agents[1].id.0, "summarizer");
    assert!(core_agents[1].token_budget.is_none());
}

#[test]
fn config_to_provider_registry() {
    let config = parse_config(TEST_CONFIG, &PathBuf::from("test.yaml")).unwrap();

    // Build provider registry from config
    let mut registry = ProviderRegistry::new();

    if let Some(openai_creds) = &config.providers.openai {
        let provider = OpenAiProvider::new(openai_creds.api_key.expose_secret(), "gpt-4o");
        registry.register(Arc::new(provider));
    }

    // Verify provider is registered and has correct capabilities
    let provider = registry.get("openai").unwrap();
    assert_eq!(provider.provider_id(), "openai");
    assert_eq!(provider.model_id(), "gpt-4o");

    let caps = provider.capabilities();
    assert!(caps.streaming);
    assert!(caps.tool_calling);
    assert!(caps.vision);
    assert_eq!(caps.max_context_tokens, 128_000);
}

#[test]
fn chat_request_from_agent_config() {
    let config = parse_config(TEST_CONFIG, &PathBuf::from("test.yaml")).unwrap();
    let agent = config.agents[0].to_core();

    // Build a ChatRequest as an agent would
    let mut messages = Vec::new();
    if let Some(sys) = &agent.system_prompt {
        messages.push(axocoatl_core::ChatMessage::system(sys));
    }
    messages.push(axocoatl_core::ChatMessage::user(
        "Research quantum computing advances in 2026",
    ));

    let request = ChatRequest {
        messages,
        tools: Vec::new(),
        max_tokens: agent.token_budget.as_ref().map(|b| b.per_call),
        temperature: None,
        top_p: None,
        response_format: None,
        stop_sequences: Vec::new(),
        provider_options: None,
        model_override: None,
    };

    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.max_tokens, Some(8192));
    assert_eq!(
        request.messages[0].text_content(),
        Some("You are a research assistant.")
    );
}

#[test]
fn token_counting_on_request() {
    use axocoatl_token::{ApproximateCounter, TokenCounter};

    let counter = ApproximateCounter::new().unwrap();

    let messages = vec![
        axocoatl_core::ChatMessage::system("You are a research assistant."),
        axocoatl_core::ChatMessage::user("Research quantum computing"),
    ];

    let token_count = counter.count_messages(&messages);
    assert!(token_count > 0);
    // Should be in a reasonable range: system + user + overhead
    assert!(token_count < 100);
    assert!(token_count > 5);
}
