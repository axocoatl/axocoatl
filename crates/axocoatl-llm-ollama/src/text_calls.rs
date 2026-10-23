//! Tool calls a model wrote as answer text instead of Ollama's structured
//! `tool_calls`, read the same way by the native and the OpenAI-compatible
//! provider.
//!
//! Qwen3-Coder sometimes samples a plain `<` instead of its `<tool_call>`
//! token. Ollama's parser lifts only calls that open with that token and
//! returns the rest as content, ended by `stop`. Only a run of call blocks in
//! Qwen3-Coder's own format that ends the answer is read as calls: the first
//! line that begins with a call tag outside a code fence or an HTML block must
//! begin it, and every block must name an offered tool with values that fit
//! its parameters. JSON is never read as a call.
//!
//! An answer that calls an offered tool in a way that cannot be run (a
//! malformed block, prose after the calls, a tool the request did not offer,
//! values that do not fit, a call written as JSON) is refused instead of
//! becoming a final answer that ran nothing: the turn needs attention and the
//! refusal says why.

use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use axocoatl_core::ResponseFormat;
use axocoatl_llm::{
    provider_tool_metadata, validate_response_tool_call, ChatRequest, ToolCall, ToolDefinition,
};
use serde_json::Value;

/// Where a call Qwen3-Coder writes as text begins.
const MARKERS: [&str; 2] = ["<function=", "<tool_call>"];
/// Calls read from one answer, and the longest answer read for them: bounds
/// on work done inside the stream.
const TEXT_CALL_LIMIT: usize = 128;
const TEXT_CALL_CONTENT_LIMIT: usize = 256 * 1024;

/// Whether a response to `request` may hold calls written as text: tools were
/// offered, the answer is not structured output, and no stop sequence of the
/// caller's can have cut the answer short (a cut answer could turn a quoted
/// example into one that ends the answer).
pub(crate) fn reads_text_calls(request: &ChatRequest) -> bool {
    !request.tools.is_empty()
        && request.stop_sequences.is_empty()
        && request.response_format != Some(ResponseFormat::Json)
}

/// What the text of a finished answer holds.
#[derive(Debug)]
pub(crate) enum TextCalls {
    /// No call to an offered tool: the text is the answer.
    Answer,
    /// The calls that end the answer, and where the answer's own text ends.
    Calls {
        answer_end: usize,
        calls: Vec<ToolCall>,
    },
    /// A call to an offered tool that cannot be run, and why.
    Unreadable(String),
}

/// Read the calls a finished answer to `request` wrote as text (see the
/// module documentation). `complete` is false when the response stopped at
/// its output limit: nothing is then read as a call, but a call it started
/// is still reported.
pub(crate) fn read_text_calls(
    content: &str,
    request: &ChatRequest,
    provider: &str,
    complete: bool,
) -> TextCalls {
    if !reads_text_calls(request) {
        return TextCalls::Answer;
    }
    let offered = |name: &str| request.tools.iter().any(|tool| tool.name == name);
    let mut blocks = Blocks::default();
    // The first call line, and where the answer's own text before it ends.
    let mut first = None;
    let mut called = None;
    let mut json_line = None;
    let mut opener = Opener::default();
    let mut at = 0;
    while let Some((line, next)) = next_line(content, at, true) {
        let open = blocks.read(line);
        if open && is_call_line(line) {
            first.get_or_insert((at, opener.end().unwrap_or(at)));
            if called.is_none() {
                called = called_tool(line, &content[at..]).filter(|name| offered(name));
            }
        } else if open && line.starts_with('{') {
            json_line = Some(at);
        }
        opener.read(line, at, open);
        at = next;
    }
    if let (Some((start, answer_end)), Some(name)) = (first, &called) {
        let why = if !complete {
            "the response reached its output limit (Ollama's done_reason \"length\") \
             before the call was complete"
                .to_owned()
        } else if content.len() > TEXT_CALL_CONTENT_LIMIT {
            "the answer is longer than 256 KiB".to_owned()
        } else {
            match recover(&content[start..], request, provider) {
                Ok(calls) => return TextCalls::Calls { answer_end, calls },
                Err(why) => why,
            }
        };
        return TextCalls::Unreadable(format!(
            "the model wrote a call to {name} as answer text that Axocoatl cannot run: {why}"
        ));
    }
    // A call written as JSON, the whole answer or the lines that end it.
    for candidate in [Some(0), json_line].into_iter().flatten() {
        if let Some(name) = json_call(content[candidate..].trim()).filter(|name| offered(name)) {
            return TextCalls::Unreadable(format!(
                "the model wrote a call to {name} as JSON in its answer, which Axocoatl never \
                 runs: only calls Ollama returns as tool calls, or that end the answer in \
                 Qwen3-Coder's <function=…> format, run"
            ));
        }
    }
    TextCalls::Answer
}

/// The calls of the run that starts `text` (the rest of the answer).
fn recover(text: &str, request: &ChatRequest, provider: &str) -> Result<Vec<ToolCall>, String> {
    let run = call_run(text)?;
    let used: HashSet<&str> = request
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .map(|call| call.id.as_str())
        .collect();
    let nonce = response_nonce();
    let mut next = 0usize;
    let mut calls = Vec::with_capacity(run.len());
    for (name, parameters) in run {
        let tool = request
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| format!("it also calls {name}, which this turn does not offer"))?;
        let mut arguments = Value::Object(
            parameters
                .into_iter()
                .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
                .collect(),
        );
        coerce_text_arguments(&mut arguments, &tool.parameters);
        fits_parameters(&arguments, tool).map_err(|why| format!("{name} {why}"))?;
        validate_response_tool_call(provider, name, &arguments, &request.tools)
            .map_err(|error| error.to_string())?;
        // An id no call in the history has, and that a later response of the
        // same Session cannot repeat even when the history it is sent with
        // has dropped this one.
        let id = loop {
            let id = format!("call_text_{nonce}_{next}");
            next += 1;
            if !used.contains(id.as_str()) {
                break id;
            }
        };
        calls.push(ToolCall {
            id,
            name: name.to_owned(),
            arguments,
            provider_metadata: provider_tool_metadata(provider),
        });
    }
    Ok(calls)
}

/// Sixteen hex digits that differ between the responses of this process
/// (and, seeded per process, between processes): two of a Session's
/// recoveries share one with a chance of about n²/2⁶⁵.
fn response_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos()),
    );
    format!("{:016x}", hasher.finish())
}

/// A `<tool_call>` that ends the last line with text outside a code fence or
/// an HTML block: when a call line follows (after blank lines), it opens the
/// call, and the answer's own text ends before it.
#[derive(Debug, Default)]
struct Opener(Option<usize>);

impl Opener {
    /// Read the line that starts at `at` (`open`: outside every block).
    fn read(&mut self, line: &str, at: usize, open: bool) {
        if open && line.trim().is_empty() {
            return;
        }
        self.0 = line
            .trim_end()
            .strip_suffix("<tool_call>")
            .filter(|_| open)
            .map(|before| at + before.len());
    }

    fn end(&self) -> Option<usize> {
        self.0
    }
}

/// The tool a call line names: `<function=NAME>`, alone or after
/// `<tool_call>`, or the `name` of the JSON object a `<tool_call>` holds.
/// `rest` is the answer from the line on.
fn called_tool(line: &str, rest: &str) -> Option<String> {
    let after_tag = line.strip_prefix("<tool_call>").map(str::trim_start);
    if let Some((name, _)) = after_tag
        .unwrap_or(line)
        .strip_prefix("<function=")
        .and_then(|tail| tail.split_once('>'))
    {
        return Some(name.to_owned());
    }
    after_tag?;
    let body = rest.strip_prefix("<tool_call>")?;
    let body = body
        .split_once("</tool_call>")
        .map_or(body, |(body, _)| body);
    json_call(body.trim())
}

/// The tool a call written as JSON names: `{"name": NAME, "arguments"|
/// "parameters": {…}}` or `{NAME: {…}}`.
fn json_call(text: &str) -> Option<String> {
    if !text.starts_with('{') || !text.ends_with('}') {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    match object.get("name") {
        Some(Value::String(name))
            if ["arguments", "parameters"]
                .iter()
                .any(|key| object.get(*key).is_some_and(Value::is_object)) =>
        {
            Some(name.clone())
        }
        _ if object.len() == 1 => object
            .iter()
            .next()
            .filter(|(_, arguments)| arguments.is_object())
            .map(|(name, _)| name.clone()),
        _ => None,
    }
}

/// A call block's tool name and its parameters, as written.
type TextCall<'a> = (&'a str, Vec<(&'a str, &'a str)>);

/// One `<function=NAME>` block of Qwen3-Coder's call format at the start of
/// `text`: its name, its `<parameter=KEY>VALUE</parameter>` pairs and its
/// length. `None` unless only whitespace and parameter pairs come between the
/// opening tag and `</function>`. A value is text up to `</parameter>`, never
/// read for further calls, and holds no line that opens or closes a tag (a
/// value missing its `</parameter>` would otherwise swallow the next call);
/// one newline on each side is the format's own.
fn function_block(text: &str) -> Option<(TextCall<'_>, usize)> {
    let rest = text.strip_prefix("<function=")?;
    let (name, mut body) = rest.split_once('>')?;
    if !plain_tag_word(name) {
        return None;
    }
    let mut parameters: Vec<(&str, &str)> = Vec::new();
    let mut keys = HashSet::new();
    loop {
        body = body.trim_start();
        if let Some(after) = body.strip_prefix("</function>") {
            return Some(((name, parameters), text.len() - after.len()));
        }
        let (key, after) = body.strip_prefix("<parameter=")?.split_once('>')?;
        let (value, after) = after.split_once("</parameter>")?;
        if !plain_tag_word(key) || !keys.insert(key) || swallows_markup(value) {
            return None;
        }
        let value = value
            .strip_prefix("\r\n")
            .or_else(|| value.strip_prefix('\n'))
            .unwrap_or(value);
        let value = value
            .strip_suffix("\r\n")
            .or_else(|| value.strip_suffix('\n'))
            .unwrap_or(value);
        parameters.push((key, value));
        body = after;
    }
}

/// Whether a parameter value holds markup of the call around it rather than
/// text: a line that opens or closes a tag, another parameter, or a closed
/// call followed by another (a value missing its `</parameter>` takes in
/// what comes after it, on the same line or the next).
fn swallows_markup(value: &str) -> bool {
    let tags = [
        "<function=",
        "</function>",
        "<parameter=",
        "<tool_call>",
        "</tool_call>",
    ];
    value
        .lines()
        .any(|line| tags.iter().any(|tag| line.trim_start().starts_with(tag)))
        || value.contains("<parameter=")
        || value
            .split_once("</function>")
            .is_some_and(|(_, after)| after.contains("<function=") || after.contains("<tool_call>"))
}

/// A tool or parameter name inside a tag: no whitespace or markup.
fn plain_tag_word(word: &str) -> bool {
    !word.is_empty()
        && !word
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '`'))
}

/// The `<function=…>` blocks `text` consists of, with only whitespace and
/// `<tool_call>` tags around them, or why it is not such a run.
fn call_run(mut text: &str) -> Result<Vec<TextCall<'_>>, String> {
    let mut blocks = Vec::new();
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return Ok(blocks);
        }
        if let Some(after) = text
            .strip_prefix("<tool_call>")
            .or_else(|| text.strip_prefix("</tool_call>"))
        {
            text = after;
            continue;
        }
        if text.starts_with('{') {
            return Err(
                "the call is written as JSON in a <tool_call> block, which Axocoatl \
                        never runs as a call"
                    .into(),
            );
        }
        if !text.starts_with("<function=") {
            return Err("text follows the call markup, so it does not end the answer".into());
        }
        let (block, used) = function_block(text).ok_or_else(|| {
            "a call block is not well formed: each <parameter=NAME> must be closed by \
             </parameter>, given once and hold no other tag, and the block closed by \
             </function>"
                .to_owned()
        })?;
        blocks.push(block);
        if blocks.len() > TEXT_CALL_LIMIT {
            return Err(format!("it holds more than {TEXT_CALL_LIMIT} calls"));
        }
        text = &text[used..];
    }
}

/// A line the call format starts: a call tag at its very start.
fn is_call_line(line: &str) -> bool {
    MARKERS.iter().any(|marker| line.starts_with(marker))
}

/// The line of `text` that starts at `from` (without its ending) and where
/// the next line starts. Lines end as Markdown ends them: at `\n`, `\r\n` or
/// a lone `\r`. Unless `complete`, a last line that has not ended yet (or
/// whose `\r` a `\n` may still follow) is not returned.
fn next_line(text: &str, from: usize, complete: bool) -> Option<(&str, usize)> {
    let rest = text.get(from..).filter(|rest| !rest.is_empty())?;
    let Some(at) = rest.find(['\n', '\r']) else {
        return complete.then_some((rest, text.len()));
    };
    let next = match (rest.as_bytes()[at], rest.as_bytes().get(at + 1)) {
        (b'\r', Some(b'\n')) => at + 2,
        (b'\r', None) if !complete => return None,
        _ => at + 1,
    };
    Some((&rest[..at], from + next))
}

/// The code fence or HTML block the lines read so far leave open. Inside one,
/// call markup is quoted text, never a call.
#[derive(Debug, Clone, Default)]
struct Blocks {
    fence: Option<(char, usize)>,
    html: Option<HtmlEnd>,
}

/// What ends an HTML block of CommonMark's kinds 1 to 5, which run until it
/// whatever lines come between: a closing tag (any case), or text.
#[derive(Debug, Clone, Copy)]
enum HtmlEnd {
    Tag(&'static str),
    Text(&'static str),
}

impl HtmlEnd {
    fn ends(self, text: &str) -> bool {
        match self {
            HtmlEnd::Tag(tag) => text.to_ascii_lowercase().contains(tag),
            HtmlEnd::Text(end) => text.contains(end),
        }
    }
}

impl Blocks {
    /// Read the next line: whether it lies outside every code fence and HTML
    /// block, where a call line can start.
    fn read(&mut self, line: &str) -> bool {
        if let Some(end) = self.html {
            if end.ends(line) {
                self.html = None;
            }
            return false;
        }
        if let Some((mark, length, bare)) = fence_line(line) {
            self.fence = match self.fence {
                None => Some((mark, length)),
                Some((open, need)) if open == mark && length >= need && bare => None,
                open => open,
            };
            return false;
        }
        if self.fence.is_some() {
            return false;
        }
        if let Some((end, after)) = html_start(line) {
            if !end.ends(after) {
                self.html = Some(end);
            }
            return false;
        }
        true
    }

    fn open(&self) -> bool {
        self.fence.is_none() && self.html.is_none()
    }
}

/// A code fence line as Markdown reads it: up to three spaces, then three or
/// more backticks (with no backtick after them) or tildes. Its character and
/// length, and whether only spaces and tabs follow (so it can close a fence).
fn fence_line(line: &str) -> Option<(char, usize, bool)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = line.len() - line.trim_start_matches(mark).len();
    let info = &line[length..];
    if length < 3 || (mark == '`' && info.contains('`')) {
        return None;
    }
    Some((mark, length, info.trim_matches([' ', '\t']).is_empty()))
}

/// The start of an HTML block that runs to an end condition (CommonMark's
/// kinds 1 to 5: `<pre`, `<script`, `<style`, `<textarea`, `<!--`, `<?`,
/// `<!X`, `<![CDATA[`): what ends it, and the text of the line after the
/// start, where the same line can end it.
fn html_start(line: &str) -> Option<(HtmlEnd, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    for (tag, end) in [
        ("<pre", "</pre>"),
        ("<script", "</script>"),
        ("<style", "</style>"),
        ("<textarea", "</textarea>"),
    ] {
        if line
            .get(..tag.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(tag))
            && line[tag.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == '>' || c.is_ascii_whitespace())
        {
            return Some((HtmlEnd::Tag(end), line));
        }
    }
    for (start, end) in [("<!--", "-->"), ("<?", "?>"), ("<![CDATA[", "]]>")] {
        if let Some(after) = line.strip_prefix(start) {
            return Some((HtmlEnd::Text(end), after));
        }
    }
    let after = line.strip_prefix("<!")?;
    after
        .starts_with(|c: char| c.is_ascii_alphabetic())
        .then_some((HtmlEnd::Text(">"), after))
}

/// How much of a streamed answer can be shown while it is read, when calls
/// written as text may end it. Text is shown as it comes; from the first call
/// line outside a code fence or an HTML block on, it is held until the
/// response ends and shows whether it was a call. A line that has not ended
/// and could still begin a call tag is held until it does, and so is a
/// `<tool_call>` that ends a line until the next line shows whether it
/// opens a call.
#[derive(Debug, Default)]
pub(crate) struct TextHold {
    released: usize,
    /// Where the first line not yet read starts.
    scanned: usize,
    blocks: Blocks,
    /// Where the first call line starts.
    call_line: Option<usize>,
    opener: Opener,
}

impl TextHold {
    /// The text of `content` that can be shown now and was not yet.
    pub(crate) fn release(&mut self, content: &str) -> Option<String> {
        if let Some(start) = self.call_line {
            return self.release_to(content, self.opener.end().unwrap_or(start));
        }
        while let Some((line, next)) = next_line(content, self.scanned, false) {
            let open = self.blocks.read(line);
            if open && is_call_line(line) {
                self.call_line = Some(self.scanned);
                return self.release_to(content, self.opener.end().unwrap_or(self.scanned));
            }
            self.opener.read(line, self.scanned, open);
            self.scanned = next;
        }
        let partial = &content[self.scanned..];
        let may_call = self.blocks.open()
            && MARKERS
                .iter()
                .any(|marker| marker.starts_with(partial) || partial.starts_with(marker));
        // A line that has begun with text other than a call tag is no call
        // line, so a `<tool_call>` before it opened none.
        if !may_call && !partial.trim().is_empty() {
            self.opener = Opener::default();
        }
        let end = if may_call {
            self.scanned
        } else {
            content.len()
        };
        self.release_to(
            content,
            self.opener.end().map_or(end, |opener| opener.min(end)),
        )
    }

    /// The text of `content` up to `end` that was not shown yet.
    pub(crate) fn release_to(&mut self, content: &str, end: usize) -> Option<String> {
        if end <= self.released {
            return None;
        }
        let delta = content[self.released..end].to_owned();
        self.released = end;
        Some(delta)
    }
}

/// Give each text value of a call written as text the type the tool's schema
/// declares for it, as Ollama's Qwen3-Coder parser does for the calls it
/// lifts. A value that does not parse stays text.
pub(crate) fn coerce_text_arguments(arguments: &mut Value, schema: &Value) {
    let (Some(arguments), Some(properties)) = (
        arguments.as_object_mut(),
        schema.get("properties").and_then(Value::as_object),
    ) else {
        return;
    };
    for (key, value) in arguments.iter_mut() {
        let declared = declared_types(properties.get(key));
        if declared.is_empty() {
            continue;
        }
        if let Some(typed) = value.as_str().and_then(|text| typed_text(text, &declared)) {
            *value = typed;
        }
    }
}

/// The types a property's schema declares.
fn declared_types(property: Option<&Value>) -> Vec<&str> {
    match property.and_then(|property| property.get("type")) {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// `text` as a value of a declared type it parses as. A number must be a JSON
/// number literal: no sign `+`, no leading zero (`0755` stays text rather
/// than become 755), no `NaN`. An integer outside the 64-bit range stays text
/// rather than lose digits as a float.
fn typed_text(text: &str, declared: &[&str]) -> Option<Value> {
    let text = text.trim();
    let integer = declared.contains(&"integer");
    let number = declared.contains(&"number");
    if integer || number {
        if let Some(value) = json_number(text, number) {
            return Some(value);
        }
    }
    if declared.contains(&"boolean") {
        if text.eq_ignore_ascii_case("true") {
            return Some(true.into());
        }
        if text.eq_ignore_ascii_case("false") {
            return Some(false.into());
        }
    }
    if declared.contains(&"null") && text.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if declared.contains(&"array") || declared.contains(&"object") {
        if let Ok(json) = serde_json::from_str::<Value>(text) {
            if (json.is_array() && declared.contains(&"array"))
                || (json.is_object() && declared.contains(&"object"))
            {
                return Some(json);
            }
        }
    }
    None
}

/// `text` as a JSON number: an integer in the i64 or u64 range, or, when
/// `fraction` allows one, any finite JSON number literal.
fn json_number(text: &str, fraction: bool) -> Option<Value> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let whole_end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let whole = &digits[..whole_end];
    if whole.is_empty() || (whole.len() > 1 && whole.starts_with('0')) {
        return None;
    }
    if whole_end == digits.len() {
        return text
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| text.parse::<u64>().map(Value::from))
            .ok();
    }
    // A fraction and an exponent, as JSON writes them.
    let mut rest = &digits[whole_end..];
    if let Some(after) = rest.strip_prefix('.') {
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if end == 0 {
            return None;
        }
        rest = &after[end..];
    }
    if let Some(after) = rest.strip_prefix(['e', 'E']) {
        let after = after.strip_prefix(['+', '-']).unwrap_or(after);
        if after.is_empty() || !after.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        rest = "";
    }
    if !rest.is_empty() || !fraction {
        return None;
    }
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

/// Whether recovered `arguments` fit the tool's parameters as its schema
/// declares them: every required parameter given, no undeclared one when
/// `additionalProperties` is false, each value of a declared type and among
/// a declared `enum`. Why not, otherwise.
fn fits_parameters(arguments: &Value, tool: &ToolDefinition) -> Result<(), String> {
    let Some(arguments) = arguments.as_object() else {
        return Err("has no parameters object".into());
    };
    let schema = &tool.parameters;
    let properties = schema.get("properties").and_then(Value::as_object);
    for required in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !arguments.contains_key(required) {
            return Err(format!("misses its required parameter {required}"));
        }
    }
    for (key, value) in arguments {
        let property = properties.and_then(|properties| properties.get(key));
        if property.is_none() && schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            return Err(format!("has a parameter {key} it does not declare"));
        }
        let declared = declared_types(property);
        if !declared.is_empty() && !declared.iter().any(|kind| has_type(value, kind)) {
            return Err(format!(
                "gives {key} a value that is not {}",
                declared.join(" or ")
            ));
        }
        if let Some(allowed) = property
            .and_then(|property| property.get("enum"))
            .and_then(Value::as_array)
        {
            if !allowed.contains(value) {
                return Err(format!("gives {key} a value its schema does not allow"));
            }
        }
    }
    Ok(())
}

/// Whether `value` is of the JSON Schema type `kind`.
fn has_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        // A type this check does not know is left to the tool.
        _ => true,
    }
}

#[cfg(test)]
mod tests;
