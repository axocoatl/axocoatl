//! Refuse a terminal Qwen function envelope returned as OpenRouter content.
//! This deliberately does not recover calls: native ids and reasoning
//! metadata must come from the provider. Quoted or ambiguous text remains
//! an answer, and a native call always takes precedence at the caller.
use axocoatl_core::ResponseFormat;
use axocoatl_llm::ChatRequest;

const TEXT_LIMIT: usize = 256 * 1024;

/// A bounded copy for recognizing the observed wrong-channel response.
/// Oversized or structured/stopped text is not interpreted.
pub(super) struct Detector(Option<String>);

impl Detector {
    pub(super) fn new(request: &ChatRequest) -> Self {
        Self(
            (!request.tools.is_empty()
                && request.stop_sequences.is_empty()
                && request.response_format != Some(ResponseFormat::Json))
            .then(String::new),
        )
    }

    pub(super) fn push(&mut self, text: &str) {
        if let Some(content) = &mut self.0 {
            if text.len() > TEXT_LIMIT.saturating_sub(content.len()) {
                self.0 = None;
            } else {
                content.push_str(text);
            }
        }
    }

    pub(super) fn called_tool<'a>(&'a self, request: &ChatRequest) -> Option<&'a str> {
        let content = self.0.as_deref()?.trim_end();
        let mut offset = 0;
        let mut opener = None;
        for raw in content.split_inclusive('\n') {
            let line = raw.trim_end_matches(['\n', '\r']);
            if line == "<tool_call>" {
                opener = Some(offset);
            } else if let Some(name) = tag(line, "function") {
                let start = opener.unwrap_or(offset);
                let before = content[..start].trim_end();
                // Do not reinterpret a quoted/fenced/HTML example or a
                // prose introduction such as "The format is:". This is a
                // conservative detector: even an earlier closed quotation
                // makes the response ambiguous and leaves it as text.
                if before.contains(['`', '~', '<', '>']) || before.ends_with(':') {
                    return None;
                }
                if request.tools.iter().any(|tool| tool.name == name)
                    && envelope(&content[offset..])
                {
                    return Some(name);
                }
                return None;
            } else if !line.trim().is_empty() {
                opener = None;
            }
            offset += raw.len();
        }
        None
    }
}

fn tag<'a>(line: &'a str, kind: &str) -> Option<&'a str> {
    let name = line
        .strip_prefix('<')?
        .strip_prefix(kind)?
        .strip_prefix('=')?
        .strip_suffix('>')?;
    (!name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')))
    .then_some(name)
}

/// Only a complete standalone envelope ending the answer is recognized.
/// Values are neither parsed nor validated, and can never become arguments.
fn envelope(text: &str) -> bool {
    let mut lines = text.lines();
    if lines
        .next()
        .and_then(|line| tag(line, "function"))
        .is_none()
    {
        return false;
    }
    loop {
        let Some(line) = lines.next() else {
            return false;
        };
        if line == "</function>" {
            return match lines.next() {
                None => true,
                Some("</tool_call>") => lines.next().is_none(),
                _ => false,
            };
        }
        if tag(line, "parameter").is_none() {
            return false;
        }
        if !lines.any(|line| line == "</parameter>") {
            return false;
        }
    }
}
