//! Shared budget and cancellation guard for non-streaming actor provider calls.

use axocoatl_core::{ChatMessage, MessageRole, OverflowPolicy, TokenUsageStats};
use axocoatl_llm::{ChatRequest, ChatResponse, LlmProvider};
use axocoatl_token::{BudgetError, TokenCounter, TokenTracker};

use crate::behavior::ExecutionUsageState;
use crate::error::AgentError;
use crate::run_control::AgentRunControl;

pub(crate) enum ControlledChat {
    Response(ChatResponse),
    Cancelled,
}

fn request_context_tokens(
    counter: &dyn TokenCounter,
    request: &ChatRequest,
    output_headroom: usize,
) -> usize {
    let tool_tokens = request.tools.iter().fold(0_usize, |tokens, definition| {
        let provider_visible = serde_json::json!({
            "name": definition.name,
            "description": definition.description,
            "parameters": definition.parameters,
        });
        tokens.saturating_add(counter.count_tool_definition(&provider_visible))
    });
    counter
        .count_messages(&request.messages)
        .saturating_add(tool_tokens)
        .saturating_add(output_headroom)
}

/// Bound a coordinator-side request to the exact selected model before any
/// remote call. The current ordinary User-to-tail suffix (including current
/// attachments and an internal decomposition/synthesis prompt) is immutable.
/// Only complete, older text-only User turns may be projected out, and only at
/// an ordinary User boundary. The caller-owned Session history is untouched.
fn project_request_to_context(
    provider: &dyn LlmProvider,
    counter: &dyn TokenCounter,
    request: &mut ChatRequest,
    protected_suffix_start: usize,
) -> Result<(), AgentError> {
    if !provider.model_constraints_known(request) {
        return Ok(());
    }
    let capabilities = provider.capabilities_for(request);
    let limit = capabilities.max_context_tokens;
    if limit == 0 {
        return Ok(());
    }
    let output_headroom = request.max_tokens.unwrap_or(capabilities.max_output_tokens);
    let required = request_context_tokens(counter, request, output_headroom);
    if required <= limit {
        return Ok(());
    }

    if protected_suffix_start >= request.messages.len()
        || request.messages[protected_suffix_start].role != MessageRole::User
    {
        return Err(AgentError::Internal(
            "coordinator context projection requires a current User boundary".to_string(),
        ));
    }

    let leading_system_count = request
        .messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count();
    if leading_system_count > protected_suffix_start {
        return Err(AgentError::Internal(
            "coordinator current User boundary overlaps its system context".to_string(),
        ));
    }

    // Coordinator request history is sanitized before it reaches this seam.
    // Defend the boundary anyway: never try to compact native tool protocol
    // messages or an unexpected embedded System message by role inference.
    let older_history = &request.messages[leading_system_count..protected_suffix_start];
    let safe_text_prefix = older_history.iter().all(|message| {
        matches!(message.role, MessageRole::User | MessageRole::Assistant)
            && message.tool_calls.is_empty()
            && message.tool_call_id.is_none()
            && message.name.is_none()
    });
    if !safe_text_prefix {
        return Err(AgentError::ContextLimitExceeded { required, limit });
    }

    let original = request.messages.clone();
    let mut cuts = (leading_system_count..protected_suffix_start)
        .filter(|index| original[*index].role == MessageRole::User)
        .collect::<Vec<_>>();
    // Dropping the complete older prefix is always the final safe candidate.
    cuts.push(protected_suffix_start);
    cuts.dedup();

    let leading_system = &original[..leading_system_count];
    for cut in cuts {
        let mut candidate = Vec::with_capacity(
            leading_system
                .len()
                .saturating_add(original.len().saturating_sub(cut)),
        );
        candidate.extend_from_slice(leading_system);
        candidate.extend_from_slice(&original[cut..]);
        request.messages = candidate;
        let candidate_required = request_context_tokens(counter, request, output_headroom);
        if candidate_required <= limit {
            return Ok(());
        }
    }

    let protected_required = request_context_tokens(counter, request, output_headroom);
    Err(AgentError::ContextLimitExceeded {
        required: protected_required,
        limit,
    })
}

fn preflight_provider_spend(
    provider: &dyn LlmProvider,
    tracker: Option<&TokenTracker>,
    request: &mut ChatRequest,
) -> Result<usize, AgentError> {
    let estimated_input = provider.count_tokens(request);
    let Some(tracker) = tracker else {
        return Ok(estimated_input);
    };

    let provider_default_output =
        if request.max_tokens.is_none() && provider.model_constraints_known(request) {
            provider.capabilities_for(request).max_output_tokens
        } else {
            0
        };
    let output_reservation = request.max_tokens.unwrap_or_else(|| {
        if tracker.budget().overflow_policy != OverflowPolicy::Abort {
            return provider_default_output;
        }

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
                    "Coordinator provider call would exceed token budget, continuing (warn policy)"
                );
            }
        }
    }
    Ok(estimated_input)
}

fn message_segment_tokens(counter: &dyn TokenCounter, messages: &[ChatMessage]) -> usize {
    let reply_priming = counter.count_messages(&[]);
    counter
        .count_messages(messages)
        .saturating_sub(reply_priming)
}

fn estimated_response_output_tokens(counter: &dyn TokenCounter, response: &ChatResponse) -> usize {
    let mut assistant = ChatMessage::assistant(&response.content);
    assistant.tool_calls = response.tool_calls.clone();
    let structured = message_segment_tokens(counter, &[assistant]);
    let explicit =
        response
            .tool_calls
            .iter()
            .fold(counter.count_text(&response.content), |tokens, call| {
                let provider_output = serde_json::json!({
                    "id": &call.id,
                    "name": &call.name,
                    "arguments": &call.arguments,
                });
                tokens.saturating_add(counter.count_text(&provider_output.to_string()))
            });
    structured.max(explicit)
}

fn record_provider_usage(
    tracker: Option<&TokenTracker>,
    usage: &TokenUsageStats,
) -> Result<(), AgentError> {
    let Some(tracker) = tracker else {
        return Ok(());
    };
    let reported_total = usage.total();
    let tracked_output = usage
        .output_tokens
        .saturating_add(usage.reasoning_tokens.unwrap_or(0));
    let per_call_overrun =
        (reported_total > tracker.budget().per_call).then_some(AgentError::TokenBudgetExceeded {
            used: reported_total,
            budget: tracker.budget().per_call,
        });
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
                    "Coordinator provider-reported call usage exceeded token budget (warn policy)"
                );
            }
            if let Err(error) = recorded {
                tracing::warn!(error = %error, "Coordinator provider-reported usage exceeded execution token budget (warn policy)");
            }
            Ok(())
        }
    }
}

pub(crate) async fn chat(
    provider: &dyn LlmProvider,
    counter: &dyn TokenCounter,
    tracker: Option<&TokenTracker>,
    usage_accumulator: Option<&ExecutionUsageState>,
    mut request: ChatRequest,
    protected_suffix_start: usize,
    control: Option<&AgentRunControl>,
) -> Result<ControlledChat, AgentError> {
    if control.is_some_and(AgentRunControl::is_cancelled) {
        return Ok(ControlledChat::Cancelled);
    }
    project_request_to_context(provider, counter, &mut request, protected_suffix_start)?;
    let estimated_input = preflight_provider_spend(provider, tracker, &mut request)?;
    let response = match control {
        Some(control) => {
            tokio::select! {
                biased;
                _ = control.cancelled() => return Ok(ControlledChat::Cancelled),
                response = async {
                    if let Some(accumulator) = usage_accumulator {
                        accumulator.begin_provider_call();
                    }
                    provider.chat(request).await
                } => response,
            }
        }
        None => {
            if let Some(accumulator) = usage_accumulator {
                accumulator.begin_provider_call();
            }
            provider.chat(request).await
        }
    }
    .map_err(|error| AgentError::Provider(error.to_string()))?;

    let mut response = response;
    if response.usage.total() == 0 {
        response.usage = TokenUsageStats::new(
            estimated_input,
            estimated_response_output_tokens(counter, &response),
        );
    }
    if let Some(accumulator) = usage_accumulator {
        accumulator.record_provider_response(&response.usage);
    }
    record_provider_usage(tracker, &response.usage)?;
    Ok(ControlledChat::Response(response))
}
