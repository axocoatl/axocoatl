//! Google Gemini provider — uses the Gemini REST API (generateContent).

use std::pin::Pin;

use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

/// Translate `ContentPart`s into Gemini's native parts array. Text becomes
/// `{"text": "..."}`; data-URL images become `{"inline_data": { mime_type, data }}`.
fn gemini_parts(parts: &[axocoatl_core::ContentPart]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            axocoatl_core::ContentPart::Text(s) => {
                out.push(serde_json::json!({"text": s}));
            }
            axocoatl_core::ContentPart::Image { url, .. } => {
                if let Some(idx) = url.find("base64,") {
                    let head = &url[..idx];
                    let mime = head
                        .trim_start_matches("data:")
                        .trim_end_matches(';')
                        .to_string();
                    let data = &url[idx + "base64,".len()..];
                    out.push(serde_json::json!({
                        "inline_data": { "mime_type": mime, "data": data }
                    }));
                }
            }
        }
    }
    out
}

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Google Gemini provider using the generateContent REST API.
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn endpoint_for(&self, model: &str) -> String {
        format!(
            "{}/{}:generateContent?key={}",
            GEMINI_API_BASE, model, self.api_key
        )
    }
}

#[async_trait::async_trait]
impl LlmProvider for GeminiProvider {
    fn provider_id(&self) -> &str {
        "gemini"
    }
    fn model_id(&self) -> &str {
        &self.model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: true,
            reasoning: self.model.contains("thinking"),
            embeddings: false,
            max_context_tokens: 1_000_000, // Gemini 1.5 Pro
            max_output_tokens: 8_192,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // Gemini uses a different message format: "contents" with "parts"
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in &request.messages {
            // For User messages with multimodal Parts, build Gemini's native
            // parts array: text + inline_data { mime_type, data }.
            let parts_json: Vec<serde_json::Value> = if matches!(msg.role, MessageRole::User) {
                if let MessageContent::Parts(parts) = &msg.content {
                    gemini_parts(parts)
                } else {
                    vec![serde_json::json!({"text": match &msg.content {
                        MessageContent::Text(s) => s.clone(),
                        _ => String::new(),
                    }})]
                }
            } else {
                let text = match &msg.content {
                    MessageContent::Text(s) => s.clone(),
                    MessageContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            axocoatl_core::ContentPart::Text(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                vec![serde_json::json!({"text": text})]
            };

            match msg.role {
                MessageRole::System => {
                    system_instruction = Some(serde_json::json!({ "parts": parts_json }));
                }
                MessageRole::User => {
                    contents.push(serde_json::json!({ "role": "user", "parts": parts_json }));
                }
                MessageRole::Assistant => {
                    contents.push(serde_json::json!({ "role": "model", "parts": parts_json }));
                }
                MessageRole::Tool => {
                    contents.push(serde_json::json!({ "role": "user", "parts": parts_json }));
                }
            }
        }

        let mut body = serde_json::json!({"contents": contents});
        if let Some(sys) = system_instruction {
            body["system_instruction"] = sys;
        }
        let mut gen_config = serde_json::Map::new();
        if let Some(max) = request.max_tokens {
            gen_config.insert("maxOutputTokens".to_string(), serde_json::json!(max));
        }
        if let Some(temp) = request.temperature {
            gen_config.insert("temperature".to_string(), serde_json::json!(temp));
        }
        if !gen_config.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(gen_config);
        }

        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        let response = self
            .client
            .post(self.endpoint_for(model_for_call))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if status == 429 {
            return Err(ProviderError::RateLimited {
                provider: "gemini".to_string(),
                retry_after_secs: None,
            });
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let resp: serde_json::Value =
            response.json().await.map_err(|e| ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: 200,
                message: e.to_string(),
            })?;

        let content = resp["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let finish_reason = match resp["candidates"][0]["finishReason"].as_str() {
            Some("STOP") => FinishReason::Stop,
            Some("MAX_TOKENS") => FinishReason::MaxTokens,
            Some("SAFETY") => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        };

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason,
            usage: TokenUsageStats {
                input_tokens: resp["usageMetadata"]["promptTokenCount"]
                    .as_u64()
                    .unwrap_or(0) as usize,
                output_tokens: resp["usageMetadata"]["candidatesTokenCount"]
                    .as_u64()
                    .unwrap_or(0) as usize,
                reasoning_tokens: None,
            },
            model: self.model.clone(),
            provider: "gemini".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "Streaming not yet implemented for Gemini".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_identity() {
        let p = GeminiProvider::new("key", "gemini-2.0-flash");
        assert_eq!(p.provider_id(), "gemini");
        assert_eq!(p.model_id(), "gemini-2.0-flash");
    }

    #[test]
    fn capabilities() {
        let p = GeminiProvider::new("key", "gemini-2.0-flash");
        let caps = p.capabilities();
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 1_000_000);
    }

    #[test]
    fn endpoint_format() {
        let p = GeminiProvider::new("test-key", "gemini-2.0-flash");
        let url = p.endpoint_for("gemini-2.0-flash");
        assert!(url.contains("gemini-2.0-flash:generateContent"));
        assert!(url.contains("key=test-key"));
    }
}
