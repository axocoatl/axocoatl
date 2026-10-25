//! Completed responses whose output an Agent cannot act on
//! ([`ProviderError::UnusableResponse`]), worded once for every provider:
//! for the record (`message`, about the model) and for the model itself
//! (`note`, which an Agent sends when it asks once more).

use crate::error::ProviderError;
use crate::tools::ToolDefinition;

/// How the refusal of a completed response with no answer and no tool call
/// begins, after `<provider> returned a response that was refused: `.
pub const NO_ANSWER: &str = "the model's response held no answer and no tool call";

/// How the refusal of a call to a tool the request did not offer begins.
pub const UNOFFERED_CALL: &str = "the model called ";

/// A completed response with no answer and no tool call, as its provider
/// reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyResponse<'a> {
    /// The model wrote reasoning (and nothing else).
    pub reasoned: bool,
    /// The output tokens the provider counted for it, when it reported them
    /// (reasoning included).
    pub output_tokens: Option<usize>,
    /// It stopped at its output limit: the request's `max_tokens`, when it
    /// set one.
    pub at_limit: Option<Option<usize>>,
    /// How the provider says it ended: `Ollama ended it with done_reason
    /// "stop"`.
    pub ended: &'a str,
    /// The tools the request offered.
    pub tools: usize,
}

impl EmptyResponse<'_> {
    /// The refusal of this response by `provider`: what it held, the output
    /// tokens counted for it when they were (one output token and no
    /// reasoning is a response ended at once, as an Ollama runner that failed
    /// answers, for example when the GPU ran out of memory), how it ended and
    /// how many tools were offered; and the note that tells the model so.
    pub fn refusal(&self, provider: &str) -> ProviderError {
        let limit = |whose: &str, at: &Option<usize>| match at {
            Some(tokens) => format!("{whose} output limit of {}", count_of(*tokens, "token")),
            None => format!("{whose} output limit"),
        };
        let mut said = Vec::new();
        if self.reasoned {
            said.push("it wrote only reasoning".to_owned());
        }
        if let Some(at) = &self.at_limit {
            said.push(format!("it stopped at {}", limit("its", at)));
        }
        if let Some(output) = self.output_tokens {
            said.push(count_of(output, "output token"));
        }
        let message = format!(
            "{NO_ANSWER}{}. Nothing from this response ran; {} and {} offered",
            if said.is_empty() {
                String::new()
            } else {
                format!(" ({})", said.join("; "))
            },
            self.ended,
            count_of(self.tools, "tool")
        );
        let note = match (&self.at_limit, self.reasoned) {
            (Some(at), true) => format!(
                "you spent {} on reasoning and wrote no answer and no tool call; reason briefly \
                 this time",
                limit("this response's", at)
            ),
            (Some(at), false) => format!(
                "your response stopped at {} with no answer and no tool call",
                limit("this response's", at)
            ),
            (None, true) => "your response held only reasoning: no answer and no tool call".into(),
            (None, false) => "your response was empty: no answer and no tool call".into(),
        };
        ProviderError::UnusableResponse {
            provider: provider.to_owned(),
            message,
            note,
        }
    }
}

/// The refusal by `provider` of a response that calls `name`, a tool
/// `offered` does not hold, under a name that cannot be answered with a
/// tool error (gpt-oss's own `browser.search`): no provider takes such a
/// name back in history. It names the call and the tools the model has.
pub fn unoffered_call(provider: &str, name: &str, offered: &[ToolDefinition]) -> ProviderError {
    let name: String = name
        .chars()
        .filter(|ch| !ch.is_control())
        .take(64)
        .collect();
    let names = offered
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let (has, have) = if offered.is_empty() {
        ("it was offered none".to_owned(), "you have none".to_owned())
    } else {
        (format!("it has {names}"), format!("you have {names}"))
    };
    ProviderError::UnusableResponse {
        provider: provider.to_owned(),
        message: format!(
            "{UNOFFERED_CALL}{name}, a tool it does not have ({has}). Nothing from this \
             response ran"
        ),
        note: format!("you called {name}, a tool you do not have ({have})"),
    }
}

/// `count` of `noun` in words, digits grouped: `1 tool`, `4,096 tokens`.
pub fn count_of(count: usize, noun: &str) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{grouped} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worded(error: ProviderError) -> (String, String) {
        match error {
            ProviderError::UnusableResponse { message, note, .. } => (message, note),
            other => panic!("{other:?}"),
        }
    }

    /// Each kind of empty response says what it held for the record and
    /// tells the model so in its own terms; one stopped at its output limit
    /// says so to both, with the limit when the request set it.
    #[test]
    fn an_empty_response_is_worded_for_the_record_and_for_the_model() {
        let empty = |reasoned, output_tokens, at_limit| EmptyResponse {
            reasoned,
            output_tokens,
            at_limit,
            ended: "Ollama ended it with done_reason \"stop\"",
            tools: 2,
        };
        assert_eq!(
            worded(empty(true, Some(30), None).refusal("ollama")),
            (
                "the model's response held no answer and no tool call (it wrote only \
                 reasoning; 30 output tokens). Nothing from this response ran; Ollama ended it \
                 with done_reason \"stop\" and 2 tools offered"
                    .into(),
                "your response held only reasoning: no answer and no tool call".into()
            )
        );
        assert_eq!(
            worded(empty(false, None, None).refusal("ollama")),
            (
                "the model's response held no answer and no tool call. Nothing from this \
                 response ran; Ollama ended it with done_reason \"stop\" and 2 tools offered"
                    .into(),
                "your response was empty: no answer and no tool call".into()
            )
        );
        let (message, note) = worded(empty(true, Some(4096), Some(Some(4096))).refusal("ollama"));
        assert!(
            message.contains(
                "(it wrote only reasoning; it stopped at its output limit of 4,096 tokens; 4,096 \
                 output tokens)"
            ),
            "{message}"
        );
        assert_eq!(
            note,
            "you spent this response's output limit of 4,096 tokens on reasoning and wrote no \
             answer and no tool call; reason briefly this time"
        );
        let (message, note) = worded(empty(false, None, Some(None)).refusal("openrouter"));
        assert!(
            message.contains("(it stopped at its output limit)"),
            "{message}"
        );
        assert_eq!(
            note,
            "your response stopped at this response's output limit with no answer and no tool \
             call"
        );
    }

    #[test]
    fn a_call_to_an_unoffered_tool_names_it_and_the_tools_the_model_has() {
        let tool = |name: &str| ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        };
        let error = unoffered_call(
            "ollama",
            "browser.search\u{7}",
            &[tool("read_file"), tool("glob")],
        );
        assert_eq!(
            error.to_string(),
            "ollama returned a response that was refused: the model called browser.search, a \
             tool it does not have (it has read_file, glob). Nothing from this response ran"
        );
        assert_eq!(
            worded(error).1,
            "you called browser.search, a tool you do not have (you have read_file, glob)"
        );
        assert_eq!(
            worded(unoffered_call("openrouter", "browser.open", &[])).1,
            "you called browser.open, a tool you do not have (you have none)"
        );
        assert_eq!(count_of(1, "tool"), "1 tool");
        assert_eq!(count_of(1_234_567, "token"), "1,234,567 tokens");
    }
}
