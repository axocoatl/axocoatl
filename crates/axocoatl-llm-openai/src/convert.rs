//! Conversion between Axocoatl types and async-openai 0.41.3 types.

use axocoatl_core::{ChatMessage, ContentPart, MessageContent, MessageRole};
use axocoatl_llm::{
    validate_required_tool_call_id, validate_response_tool_call, FinishReason, ProviderError,
    ToolCall, ToolDefinition,
};

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestToolMessage,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    ChatCompletionRequestUserMessageContentPart, ChatCompletionTool, ChatCompletionTools,
    FunctionCall, FunctionObject, ImageDetail as OaiImageDetail, ImageUrl,
};

/// Convert Axocoatl ChatMessages to async-openai request messages.
pub fn to_openai_messages(
    messages: &[ChatMessage],
) -> Result<Vec<ChatCompletionRequestMessage>, ProviderError> {
    messages.iter().map(to_openai_message).collect()
}

fn to_openai_message(msg: &ChatMessage) -> Result<ChatCompletionRequestMessage, ProviderError> {
    // For user messages we preserve multimodal parts (text + images). Other
    // roles flatten to text since the OpenAI API doesn't accept images on
    // system/assistant/tool messages.
    if matches!(msg.role, MessageRole::User) {
        if let MessageContent::Parts(parts) = &msg.content {
            let mut content_parts: Vec<ChatCompletionRequestUserMessageContentPart> = Vec::new();
            for p in parts {
                match p {
                    ContentPart::Text(s) => {
                        content_parts.push(ChatCompletionRequestUserMessageContentPart::Text(
                            ChatCompletionRequestMessageContentPartText { text: s.clone() },
                        ));
                    }
                    ContentPart::Image { url, detail } => {
                        content_parts.push(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                            ChatCompletionRequestMessageContentPartImage {
                                image_url: ImageUrl {
                                    url: url.clone(),
                                    detail: Some(match detail {
                                        axocoatl_core::ImageDetail::Auto => OaiImageDetail::Auto,
                                        axocoatl_core::ImageDetail::Low => OaiImageDetail::Low,
                                        axocoatl_core::ImageDetail::High => OaiImageDetail::High,
                                    }),
                                },
                            },
                        ));
                    }
                }
            }
            if !content_parts.is_empty() {
                return Ok(ChatCompletionRequestMessage::User(
                    ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Array(content_parts),
                        name: None,
                    },
                ));
            }
        }
    }

    // Fallback: flatten to plain text.
    let text = match &msg.content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(s) => Some(s.clone()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };

    Ok(match msg.role {
        MessageRole::System => ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessage::from(text.as_str()),
        ),
        MessageRole::User => ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage::from(text.as_str()),
        ),
        MessageRole::Assistant => {
            // Replay the assistant's tool-call requests so the model sees its
            // own prior calls; without these the follow-up tool messages are
            // orphaned and the API rejects the request.
            let tool_calls = if msg.tool_calls.is_empty() {
                None
            } else {
                Some(
                    msg.tool_calls
                        .iter()
                        .map(|tc| {
                            ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: tc.id.clone(),
                                    function: FunctionCall {
                                        name: tc.name.clone(),
                                        arguments: serde_json::to_string(&tc.arguments)
                                            .unwrap_or_else(|_| "{}".to_string()),
                                    },
                                },
                            )
                        })
                        .collect(),
                )
            };
            // When the turn is tool-calls-only the text is empty; send `None`
            // rather than an empty string alongside `tool_calls`.
            let content = if text.is_empty() && tool_calls.is_some() {
                None
            } else {
                Some(text.into())
            };
            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                content,
                tool_calls,
                ..Default::default()
            })
        }
        MessageRole::Tool => ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
            content: text.into(),
            // Correlate the result with the call. Falls back to `name` for any
            // legacy message that predates the dedicated `tool_call_id` field.
            tool_call_id: msg
                .tool_call_id
                .clone()
                .or_else(|| msg.name.clone())
                .unwrap_or_default(),
        }),
    })
}

/// Extract tool calls from an OpenAI response choice.
/// async-openai 0.41.3: `ChatCompletionMessageToolCalls` is an enum, not a flat struct.
pub fn extract_tool_calls(
    choice: &async_openai::types::chat::ChatChoice,
    tools: &[ToolDefinition],
    provider: &str,
) -> Result<Vec<ToolCall>, ProviderError> {
    let Some(calls) = choice.message.tool_calls.as_ref() else {
        return Ok(Vec::new());
    };
    let mut normalized = Vec::with_capacity(calls.len());
    for call in calls {
        let ChatCompletionMessageToolCalls::Function(call) = call else {
            return Err(ProviderError::ApiError {
                provider: provider.to_string(),
                status: 200,
                message: "provider returned an unsupported custom tool call".to_string(),
            });
        };
        let arguments: serde_json::Value =
            serde_json::from_str(&call.function.arguments).map_err(|_| {
                ProviderError::ApiError {
                    provider: provider.to_string(),
                    status: 200,
                    message: "provider returned malformed tool-call arguments".to_string(),
                }
            })?;
        validate_required_tool_call_id(provider, &call.id)?;
        validate_response_tool_call(provider, &call.function.name, &arguments, tools)?;
        normalized.push(ToolCall {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments,
            provider_metadata: Default::default(),
        });
    }
    let mut ids = std::collections::HashSet::with_capacity(normalized.len());
    if normalized.iter().any(|call| !ids.insert(call.id.as_str())) {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider returned duplicate tool-call ids".to_string(),
        });
    }
    Ok(normalized)
}

/// Map OpenAI finish reason to Axocoatl FinishReason.
/// async-openai 0.41.3: `FinishReason` is a proper enum, not a string.
pub fn map_finish_reason(
    choice: &async_openai::types::chat::ChatChoice,
    provider: &str,
) -> Result<FinishReason, ProviderError> {
    use async_openai::types::chat::FinishReason as OaiFinishReason;

    let finish = match choice.finish_reason {
        Some(OaiFinishReason::Stop) => FinishReason::Stop,
        Some(OaiFinishReason::ToolCalls) => FinishReason::ToolUse,
        Some(OaiFinishReason::Length) => FinishReason::MaxTokens,
        Some(OaiFinishReason::ContentFilter) => FinishReason::ContentFilter,
        Some(OaiFinishReason::FunctionCall) => FinishReason::ToolUse,
        None => {
            return Err(ProviderError::ApiError {
                provider: provider.to_string(),
                status: 200,
                message: "provider response omitted its finish reason".to_string(),
            });
        }
    };
    Ok(finish)
}

/// Convert Axocoatl tool definitions into async-openai request tools.
///
/// Without attaching these to the outbound request the model never sees the
/// tools and can never emit a tool call — the bug this fixes (previously only
/// the Ollama provider sent tools).
pub fn to_openai_tools(tools: &[ToolDefinition]) -> Vec<ChatCompletionTools> {
    tools
        .iter()
        .map(|t| {
            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: t.name.clone(),
                    description: Some(t.description.clone()),
                    parameters: Some(t.parameters.clone()),
                    strict: None,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_openai_tools_produces_function_tools() {
        let tools = vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "location": { "type": "string" } },
                "required": ["location"]
            }),
            concurrency: Default::default(),
        }];

        let json = serde_json::to_value(to_openai_tools(&tools)).unwrap();
        assert_eq!(json[0]["type"], "function");
        assert_eq!(json[0]["function"]["name"], "get_weather");
        assert_eq!(json[0]["function"]["description"], "Get current weather");
        assert_eq!(json[0]["function"]["parameters"]["required"][0], "location");
    }

    #[test]
    fn to_openai_tools_empty_is_empty() {
        assert!(to_openai_tools(&[]).is_empty());
    }

    #[test]
    fn assistant_tool_calls_serialize_into_request() {
        let msg = ChatMessage::assistant_with_tool_calls(
            "",
            vec![axocoatl_core::ToolCall {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                arguments: serde_json::json!({ "location": "NYC" }),
                provider_metadata: Default::default(),
            }],
        );
        let converted = to_openai_messages(std::slice::from_ref(&msg)).unwrap();
        let json = serde_json::to_value(&converted).unwrap();

        assert_eq!(json[0]["role"], "assistant");
        assert_eq!(json[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(json[0]["tool_calls"][0]["type"], "function");
        assert_eq!(json[0]["tool_calls"][0]["function"]["name"], "get_weather");
        // Arguments are serialized as a JSON string per the OpenAI schema.
        let args = json[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(args).unwrap()["location"],
            "NYC"
        );
        // Tool-call-only turn carries no content field.
        assert!(json[0].get("content").is_none() || json[0]["content"].is_null());
    }

    #[test]
    fn tool_result_message_carries_tool_call_id() {
        let msg = ChatMessage::tool_result("{\"temp\":72}", "get_weather", "call_1");
        let converted = to_openai_messages(std::slice::from_ref(&msg)).unwrap();
        let json = serde_json::to_value(&converted).unwrap();

        assert_eq!(json[0]["role"], "tool");
        assert_eq!(json[0]["tool_call_id"], "call_1");
    }
}
