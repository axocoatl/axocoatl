//! Provider-safe aliases for Axocoatl's canonical tool names.
//!
//! MCP permits names that are longer and less restricted than several model
//! provider APIs accept. Axocoatl keeps the qualified executor key internally
//! (for routing, approval policy, evidence, and durable history) and translates
//! only at the provider request boundary.

use std::collections::{BTreeSet, HashMap, HashSet};

use axocoatl_core::MessageRole;
use axocoatl_llm::ChatRequest;
use sha2::{Digest, Sha256};

/// The common denominator for the configured providers. OpenAI and Anthropic
/// both cap function names at 64 ASCII letters, digits, underscores, or dashes.
pub const PROVIDER_TOOL_NAME_MAX_BYTES: usize = 64;

// Valid internal names beginning with this prefix are also aliased. Reserving
// the namespace prevents a crafted native tool name from occupying the alias
// generated for a different (for example, long MCP-qualified) name.
const ALIAS_PREFIX: &str = "axo_tool_";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProviderToolNameError {
    #[error("two internal tool names produced the same provider alias")]
    AliasCollision,
}

/// A request-local, reversible bijection between canonical Axocoatl tool names
/// and provider-compatible aliases.
///
/// Aliases are derived solely from the canonical name, so the same MCP tool gets
/// the same provider name across requests and process restarts. The explicit
/// reverse map prevents the model-facing alias from leaking into executor
/// dispatch, permission records, streamed evidence, or the durable transcript.
#[derive(Debug, Clone, Default)]
pub struct ProviderToolNameMap {
    internal_to_provider: HashMap<String, String>,
    provider_to_internal: HashMap<String, String>,
    advertised_provider_names: HashSet<String>,
}

impl ProviderToolNameMap {
    /// Build the name map needed by one provider request. Historical tool calls
    /// are included because providers validate replayed assistant/tool messages
    /// as well as the current declarations.
    pub fn for_request(request: &ChatRequest) -> Result<Self, ProviderToolNameError> {
        let advertised_internal_names: BTreeSet<String> =
            request.tools.iter().map(|tool| tool.name.clone()).collect();
        let mut internal_names = advertised_internal_names.clone();

        for message in &request.messages {
            internal_names.extend(message.tool_calls.iter().map(|call| call.name.clone()));
            if message.role == MessageRole::Tool {
                if let Some(name) = &message.name {
                    internal_names.insert(name.clone());
                }
            }
        }

        let mut map = Self::for_internal_names(internal_names)?;
        map.advertised_provider_names = advertised_internal_names
            .iter()
            .filter_map(|name| map.internal_to_provider.get(name).cloned())
            .collect();
        Ok(map)
    }

    fn for_internal_names(
        internal_names: impl IntoIterator<Item = String>,
    ) -> Result<Self, ProviderToolNameError> {
        let mut map = Self::default();
        for internal in internal_names {
            let provider = provider_name(&internal);
            if let Some(existing) = map
                .provider_to_internal
                .insert(provider.clone(), internal.clone())
            {
                if existing != internal {
                    return Err(ProviderToolNameError::AliasCollision);
                }
            }
            map.internal_to_provider.insert(internal, provider);
        }
        Ok(map)
    }

    /// Rewrite declarations and replayed tool messages immediately before the
    /// request crosses into a provider implementation.
    pub fn encode_request(&self, mut request: ChatRequest) -> ChatRequest {
        for tool in &mut request.tools {
            tool.name = self.encode_name_owned(std::mem::take(&mut tool.name));
        }
        for message in &mut request.messages {
            for call in &mut message.tool_calls {
                call.name = self.encode_name_owned(std::mem::take(&mut call.name));
            }
            if message.role == MessageRole::Tool {
                if let Some(name) = message.name.take() {
                    message.name = Some(self.encode_name_owned(name));
                }
            }
        }
        request
    }

    /// Restore a provider-returned name to the canonical executor key. Unknown
    /// names are retained so normal NotFound handling can report model errors.
    pub fn decode_name_owned(&self, provider_name: String) -> String {
        self.provider_to_internal
            .get(&provider_name)
            .cloned()
            .unwrap_or(provider_name)
    }

    /// Decode a text-fallback name only if it was declared on this request.
    /// Historical names are mapped for replay validity but are not callable.
    pub fn decode_advertised_name(&self, provider_name: &str) -> Option<&str> {
        if !self.advertised_provider_names.contains(provider_name) {
            return None;
        }
        self.provider_to_internal
            .get(provider_name)
            .map(String::as_str)
    }

    /// Owned form used when assembling streamed tool calls.
    pub fn decode_advertised_name_owned(&self, provider_name: String) -> Option<String> {
        self.decode_advertised_name(&provider_name)
            .map(str::to_string)
    }

    fn encode_name_owned(&self, internal_name: String) -> String {
        self.internal_to_provider
            .get(&internal_name)
            .cloned()
            .unwrap_or(internal_name)
    }
}

/// Whether a name can be sent unchanged to every configured provider family.
pub fn is_provider_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= PROVIDER_TOOL_NAME_MAX_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn provider_name(internal_name: &str) -> String {
    if is_provider_tool_name(internal_name) && !internal_name.starts_with(ALIAS_PREFIX) {
        return internal_name.to_string();
    }

    let digest = Sha256::digest(internal_name.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex.truncate(PROVIDER_TOOL_NAME_MAX_BYTES - ALIAS_PREFIX.len());
    format!("{ALIAS_PREFIX}{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::{ChatMessage, ToolCall};
    use axocoatl_llm::{ConcurrencyPolicy, ToolDefinition};

    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: "test tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: ConcurrencyPolicy::Safe,
        }
    }

    fn request(names: &[&str]) -> ChatRequest {
        let mut request = ChatRequest::simple("use a tool");
        request.tools = names.iter().map(|name| definition(name)).collect();
        request
    }

    #[test]
    fn compatible_names_are_stable_and_unchanged() {
        let request = request(&["read_file", "mcp__git__status-1"]);
        let map = ProviderToolNameMap::for_request(&request).unwrap();
        let encoded = map.encode_request(request);
        assert_eq!(encoded.tools[0].name, "read_file");
        assert_eq!(encoded.tools[1].name, "mcp__git__status-1");
    }

    #[test]
    fn long_unicode_and_punctuation_names_get_distinct_reversible_aliases() {
        let long = format!("mcp__{}__{}", "server".repeat(20), "tool".repeat(30));
        let unicode = "mcp__servidor-🦀__buscar/archivos";
        let punctuation_a = "mcp__server__issues.list?state=open";
        let punctuation_b = "mcp__server__issues/list?state=open";
        let original = [long.as_str(), unicode, punctuation_a, punctuation_b];
        let request = request(&original);
        let map = ProviderToolNameMap::for_request(&request).unwrap();
        let encoded = map.encode_request(request);

        let aliases: HashSet<_> = encoded
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(aliases.len(), original.len());
        for (tool, expected_internal) in encoded.tools.iter().zip(original) {
            assert!(is_provider_tool_name(&tool.name), "{}", tool.name);
            assert_eq!(tool.name.len(), PROVIDER_TOOL_NAME_MAX_BYTES);
            assert_eq!(
                map.decode_name_owned(tool.name.clone()),
                expected_internal.to_string()
            );
        }
    }

    #[test]
    fn aliases_do_not_collide_with_a_crafted_native_name() {
        let invalid = "mcp__server__tool.with.periods";
        let first_map = ProviderToolNameMap::for_request(&request(&[invalid])).unwrap();
        let generated = first_map.encode_request(request(&[invalid])).tools[0]
            .name
            .clone();

        let request = request(&[invalid, &generated]);
        let map = ProviderToolNameMap::for_request(&request).unwrap();
        let encoded = map.encode_request(request);
        assert_ne!(encoded.tools[0].name, encoded.tools[1].name);
        assert_ne!(encoded.tools[1].name, generated);
        assert_eq!(
            map.decode_name_owned(encoded.tools[0].name.clone()),
            invalid
        );
        assert_eq!(
            map.decode_name_owned(encoded.tools[1].name.clone()),
            generated
        );
    }

    #[test]
    fn request_history_uses_the_same_alias_and_decodes_back() {
        let internal = "mcp__servidor 🦀__issues.list";
        let mut request = request(&[internal]);
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "call-1".to_string(),
                    name: internal.to_string(),
                    arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                }],
            ));
        request
            .messages
            .push(ChatMessage::tool_result("ok", internal, "call-1"));

        let map = ProviderToolNameMap::for_request(&request).unwrap();
        let encoded = map.encode_request(request);
        let alias = encoded.tools[0].name.clone();
        assert_ne!(alias, internal);
        assert_eq!(encoded.messages[1].tool_calls[0].name, alias);
        assert_eq!(encoded.messages[2].name.as_deref(), Some(alias.as_str()));
        assert_eq!(map.decode_advertised_name(&alias), Some(internal));
        assert_eq!(map.decode_name_owned(alias), internal);
    }

    #[test]
    fn historical_but_unadvertised_name_cannot_be_text_fallback_called() {
        let historical = "mcp__old server__old/tool";
        let mut request = request(&["echo"]);
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "old".to_string(),
                    name: historical.to_string(),
                    arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                }],
            ));
        let map = ProviderToolNameMap::for_request(&request).unwrap();
        let encoded = map.encode_request(request);
        let historical_alias = encoded.messages[1].tool_calls[0].name.clone();
        assert!(map.decode_advertised_name(&historical_alias).is_none());
        assert_eq!(map.decode_name_owned(historical_alias), historical);
    }
}
