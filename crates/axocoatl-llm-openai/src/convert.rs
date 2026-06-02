//! Conversion between Axocoatl types and async-openai 0.33 types.

use axocoatl_core::{ChatMessage, ContentPart, MessageContent, MessageRole};
use axocoatl_llm::{FinishReason, ProviderError, ToolCall};

use async_openai::types::chat::{
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartImage,
    ChatCompletionRequestMessageContentPartText, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestToolMessage, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    ImageDetail as OaiImageDetail, ImageUrl,
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
            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                content: Some(text.into()),
                ..Default::default()
            })
        }
        MessageRole::Tool => ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
            content: text.into(),
            tool_call_id: msg.name.clone().unwrap_or_default(),
        }),
    })
}

/// Extract tool calls from an OpenAI response choice.
/// async-openai 0.33: `ChatCompletionMessageToolCalls` is an enum, not a flat struct.
pub fn extract_tool_calls(choice: &async_openai::types::chat::ChatChoice) -> Vec<ToolCall> {
    choice
        .message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|tc| match tc {
                    ChatCompletionMessageToolCalls::Function(func_call) => Some(ToolCall {
                        id: func_call.id.clone(),
                        name: func_call.function.name.clone(),
                        arguments: serde_json::from_str(&func_call.function.arguments)
                            .unwrap_or(serde_json::Value::Null),
                    }),
                    _ => None, // Skip custom tool calls for now
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map OpenAI finish reason to Axocoatl FinishReason.
/// async-openai 0.33: `FinishReason` is a proper enum, not a string.
pub fn map_finish_reason(choice: &async_openai::types::chat::ChatChoice) -> FinishReason {
    use async_openai::types::chat::FinishReason as OaiFinishReason;

    match choice.finish_reason {
        Some(OaiFinishReason::Stop) => FinishReason::Stop,
        Some(OaiFinishReason::ToolCalls) => FinishReason::ToolUse,
        Some(OaiFinishReason::Length) => FinishReason::MaxTokens,
        Some(OaiFinishReason::ContentFilter) => FinishReason::ContentFilter,
        Some(OaiFinishReason::FunctionCall) => FinishReason::ToolUse,
        None => FinishReason::Stop,
    }
}

/// Map async-openai errors to Axocoatl ProviderError.
pub fn map_openai_error(err: async_openai::error::OpenAIError) -> ProviderError {
    let msg = err.to_string();
    if msg.contains("429") || msg.to_lowercase().contains("rate") {
        ProviderError::RateLimited {
            provider: "openai".to_string(),
            retry_after_secs: None,
        }
    } else if msg.contains("401") || msg.to_lowercase().contains("auth") {
        ProviderError::AuthError {
            provider: "openai".to_string(),
        }
    } else {
        ProviderError::ApiError {
            provider: "openai".to_string(),
            status: 0,
            message: msg,
        }
    }
}
