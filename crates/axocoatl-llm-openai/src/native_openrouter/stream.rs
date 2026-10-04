use super::*;

/// Reasoning blocks OpenRouter documents for chat completions. A
/// `reasoning.server_tool_call` block means a server tool ran, which a native
/// request never asks for.
pub(super) fn replayable_reasoning(kind: &str) -> bool {
    matches!(
        kind,
        "reasoning.text" | "reasoning.summary" | "reasoning.encrypted"
    )
}

/// Reasoning text is merged into deltas of about this many bytes before it
/// is yielded. Models that stream one token per chunk would otherwise spend
/// most of a call's response byte bound on per-event framing.
const REASONING_DELTA_BYTES: usize = 256;

fn count(value: &Value, name: &str) -> Result<usize, ProviderError> {
    value
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| protocol(format!("terminal {name} usage missing or malformed")))
}

/// Terminal usage. Reasoning tokens are part of `completion_tokens`, billed
/// as output; they are kept apart so they can be shown, and every total
/// (grants, budgets, cost) counts them.
fn usage(value: &Value) -> Result<MeasuredTokenUsage, ProviderError> {
    let input = count(&value["prompt_tokens"], "prompt")?;
    let completion = count(&value["completion_tokens"], "completion")?;
    let reasoning = match value
        .get("completion_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
    {
        None | Some(Value::Null) => 0,
        Some(tokens) => count(tokens, "reasoning")?,
    };
    let output = completion
        .checked_sub(reasoning)
        .ok_or_else(|| protocol("reasoning usage exceeds total completion usage"))?;
    if value["total_tokens"].as_u64() != input.checked_add(completion).map(|n| n as u64) {
        return Err(protocol("terminal token totals disagree"));
    }
    Ok(MeasuredTokenUsage {
        usage: TokenUsageStats {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: (reasoning > 0).then_some(reasoning),
        },
        complete: true,
    })
}

/// A server tool (such as web search) bills beyond tokens. A native request
/// never offers one, so its use means the request's contract was not honored.
fn server_tool_ran(usage: &Value) -> bool {
    usage
        .get("server_tool_use_details")
        .filter(|v| !v.is_null())
        .is_some_and(|details| {
            [
                "web_search_requests",
                "tool_calls_executed",
                "tool_calls_requested",
            ]
            .iter()
            .any(|name| details.get(*name).and_then(Value::as_u64).unwrap_or(0) > 0)
        })
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

/// Reasoning blocks rebuilt from stream deltas. OpenRouter streams one block
/// as several deltas with the same `index` and `type` (text fragments, then
/// its signature); a block's later fields complete the same block.
#[derive(Default)]
pub(super) struct ReasoningDetails {
    blocks: Vec<serde_json::Map<String, Value>>,
    positions: BTreeMap<(u64, String), usize>,
}

impl ReasoningDetails {
    pub(super) fn push(&mut self, detail: &Value) -> Result<(), ProviderError> {
        let object = detail
            .as_object()
            .ok_or_else(|| protocol("reasoning detail is not an object"))?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| replayable_reasoning(kind))
            .ok_or_else(|| protocol("unsupported reasoning detail type"))?;
        let key = object
            .get("index")
            .and_then(Value::as_u64)
            .map(|index| (index, kind.to_owned()));
        // A block is complete once signed; more text under its index starts
        // the next block rather than altering a signed one.
        let continues = |block: &serde_json::Map<String, Value>| {
            !(block.get("signature").is_some_and(|v| !v.is_null())
                && ["text", "summary", "data"]
                    .iter()
                    .any(|field| object.get(*field).is_some_and(|v| !v.is_null())))
        };
        let Some(position) = key
            .as_ref()
            .and_then(|key| self.positions.get(key))
            .filter(|position| continues(&self.blocks[**position]))
        else {
            if let Some(key) = key {
                self.positions.insert(key, self.blocks.len());
            }
            self.blocks.push(object.clone());
            return Ok(());
        };
        let block = &mut self.blocks[*position];
        for (field, value) in object {
            if value.is_null() || matches!(field.as_str(), "type" | "index") {
                continue;
            }
            match (field.as_str(), block.get_mut(field)) {
                ("text" | "summary" | "data", Some(Value::String(existing))) => {
                    existing.push_str(
                        value
                            .as_str()
                            .ok_or_else(|| protocol("reasoning fragment is not text"))?,
                    );
                }
                (_, Some(existing)) if !existing.is_null() && existing != value => {
                    return Err(protocol("a reasoning block changed one of its fields"));
                }
                _ => {
                    block.insert(field.clone(), value.clone());
                }
            }
        }
        Ok(())
    }

    pub(super) fn encoded(&self) -> Result<Option<String>, ProviderError> {
        if self.blocks.is_empty() {
            return Ok(None);
        }
        serde_json::to_string(&self.blocks)
            .map(Some)
            .map_err(|_| protocol("reasoning details could not be retained"))
    }
}

pub(super) fn decode(
    response: reqwest::Response,
    request: ChatRequest,
    profile: NativeOpenRouterObservation,
    shape: CallShape,
    bounds: ProviderExecutionBounds,
    key: String,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let mut bytes = response.bytes_stream();
        // Reasoning streams many small chunks; the wire may run well past the
        // retained response bound, which the Session boundary enforces on
        // what is kept. One event still fits within that bound.
        let mut decoder = SseDecoder::new(MAX_STREAM_BYTES, bounds.response_bytes);
        let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
        let mut finished = None;
        let mut terminal_usage = None;
        let mut terminal_cost = None;
        let mut calls = BTreeMap::<usize, Call>::new();
        let mut reasoning = ReasoningDetails::default();
        let mut generation = None;
        let mut observed_provider = false;
        let mut sentinel = false;
        // Reasoning text not yet yielded; every other event flushes it first
        // so the stream keeps its order.
        let mut pending_reasoning = String::new();
        'wire: loop {
            let chunk = next_stream_item(&mut bytes, deadline, STREAM_IDLE_TIMEOUT, PROVIDER).await?;
            let eof = chunk.is_none();
            let frames = match chunk {
                Some(chunk) => decoder.push(&chunk.map_err(|e| network_error(&e, &[&key]))?)?,
                None => decoder.finish()?,
            };
            for frame in frames {
                if frame.data.trim() == "[DONE]" {
                    sentinel = true;
                    break 'wire;
                }
                let value: Value =
                    serde_json::from_str(&frame.data).map_err(|_| protocol("invalid SSE JSON"))?;
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    if id.is_empty()
                        || id.len() > 256
                        || generation.as_ref().is_some_and(|old| old != id)
                    {
                        Err(protocol("generation identity changed"))?;
                    }
                    generation = Some(id.to_owned());
                }
                if let Some(model) = value.get("model").and_then(Value::as_str) {
                    if model != profile.model {
                        Err(protocol("response changed the exact admitted model"))?;
                    }
                }
                if let Some(provider) = value.get("provider").and_then(Value::as_str) {
                    if provider != profile.provider_name {
                        Err(protocol("response changed the exact admitted provider"))?;
                    }
                    observed_provider = true;
                }
                if let Some(measured) = value.get("usage").filter(|v| !v.is_null()) {
                    if !pending_reasoning.is_empty() {
                        yield StreamEvent::ReasoningDelta {
                            delta: std::mem::take(&mut pending_reasoning),
                        };
                    }
                    let observed = usage(measured)?;
                    if measured.get("is_byok").and_then(Value::as_bool) != Some(false) {
                        yield StreamEvent::UsageObservation(MeasuredTokenUsage::lower_bound(
                            observed.usage.clone(),
                        ));
                        Err(protocol("BYOK is unsupported; terminal usage does not confirm the configured OpenRouter-credit route"))?;
                    }
                    if terminal_usage.as_ref().is_some_and(|old| old != &observed) {
                        Err(protocol("conflicting terminal usage"))?;
                    }
                    let cost = money::measured_cost(
                        measured
                            .get("cost")
                            .ok_or_else(|| protocol("terminal billed cost unavailable"))?,
                    )?;
                    if terminal_cost.is_some_and(|old| old != cost) {
                        Err(protocol("conflicting billed cost"))?;
                    }
                    terminal_usage = Some(observed.clone());
                    terminal_cost = Some(cost);
                    yield StreamEvent::UsageObservation(observed.clone());
                    yield StreamEvent::CostObservation {
                        cost_microunits: cost,
                    };
                    if server_tool_ran(measured) {
                        Err(protocol("OpenRouter ran a server tool the request never offered"))?;
                    }
                    let completion = observed
                        .usage
                        .output_tokens
                        .saturating_add(observed.usage.reasoning_tokens.unwrap_or(0));
                    if observed.usage.input_tokens > shape.prompt_tokens
                        || completion > shape.response_tokens
                        || cost > bounds.cost_microunits
                    {
                        Err(protocol(
                            "provider usage exceeded the admitted execution contract",
                        ))?;
                    }
                }
                // The request never opts into a service tier; another one
                // reported means the endpoint served it at that tier's terms.
                // Checked after this chunk's usage so its spend is kept.
                if let Some(tier) = value.get("service_tier").filter(|v| !v.is_null()) {
                    if !matches!(tier.as_str(), Some("default" | "standard")) {
                        Err(protocol("response used a service tier the request never chose"))?;
                    }
                }
                if value.get("error").is_some_and(|v| !v.is_null()) {
                    Err(protocol(
                        "provider reported an error; retained accounting remains partial",
                    ))?;
                }
                let choices = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .ok_or_else(|| protocol("missing response choices"))?;
                if choices.len() > 1 {
                    Err(protocol(
                        "more than one completion would exceed the approved request",
                    ))?;
                }
                if let Some(choice) = choices.first() {
                    if choice["index"].as_u64() != Some(0) {
                        Err(protocol("nonzero completion index"))?;
                    }
                    let delta = &choice["delta"];
                    for (field, what) in [
                        ("annotations", "web citations"),
                        ("images", "image output"),
                        ("audio", "audio output"),
                    ] {
                        if delta.get(field).is_some_and(|v| {
                            !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty())
                        }) {
                            Err(protocol(format!("unrequested {what} in the response")))?;
                        }
                    }
                    if let Some(text) = delta.get("reasoning").filter(|v| !v.is_null()) {
                        let text = text
                            .as_str()
                            .ok_or_else(|| protocol("reasoning text malformed"))?;
                        if finished.is_some() && !text.is_empty() {
                            Err(protocol("reasoning after terminal completion"))?;
                        }
                        pending_reasoning.push_str(text);
                        if pending_reasoning.len() >= REASONING_DELTA_BYTES {
                            yield StreamEvent::ReasoningDelta {
                                delta: std::mem::take(&mut pending_reasoning),
                            };
                        }
                    }
                    if let Some(details) = delta.get("reasoning_details").filter(|v| !v.is_null()) {
                        for detail in details
                            .as_array()
                            .ok_or_else(|| protocol("reasoning details malformed"))?
                        {
                            reasoning.push(detail)?;
                        }
                    }
                    if let Some(text) = delta.get("content").filter(|v| !v.is_null()) {
                        let text = text
                            .as_str()
                            .ok_or_else(|| protocol("nontext output unsupported"))?;
                        if finished.is_some() && !text.is_empty() {
                            Err(protocol("content after terminal completion"))?;
                        }
                        if !text.is_empty() {
                            if !pending_reasoning.is_empty() {
                                yield StreamEvent::ReasoningDelta {
                                    delta: std::mem::take(&mut pending_reasoning),
                                };
                            }
                            yield StreamEvent::TextDelta { delta: text.into() };
                        }
                    }
                    if let Some(parts) = delta.get("tool_calls").filter(|v| !v.is_null()) {
                        if finished.is_some() {
                            Err(protocol("tool delta after terminal completion"))?;
                        }
                        if !pending_reasoning.is_empty() {
                            yield StreamEvent::ReasoningDelta {
                                delta: std::mem::take(&mut pending_reasoning),
                            };
                        }
                        for part in parts
                            .as_array()
                            .ok_or_else(|| protocol("tool calls malformed"))?
                        {
                            let index = part["index"]
                                .as_u64()
                                .and_then(|n| usize::try_from(n).ok())
                                .filter(|n| *n < 128)
                                .ok_or_else(|| protocol("tool index missing or unbounded"))?;
                            if part
                                .get("type")
                                .and_then(Value::as_str)
                                .is_some_and(|kind| kind != "function")
                            {
                                Err(protocol("the model called a tool that is not a function"))?;
                            }
                            let call = calls.entry(index).or_default();
                            let id = part.get("id").and_then(Value::as_str).unwrap_or("");
                            if !id.is_empty() {
                                if !call.id.is_empty() && call.id != id {
                                    Err(protocol("tool identity changed"))?;
                                }
                                axocoatl_llm::validate_required_tool_call_id(PROVIDER, id)?;
                                call.id = id.into();
                            }
                            let function = &part["function"];
                            let name = function.get("name").and_then(Value::as_str);
                            if let Some(name) = name {
                                if !call.name.is_empty() && call.name != name {
                                    Err(protocol("tool name changed"))?;
                                }
                                call.name = name.into();
                            }
                            let arguments = function
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            call.args.push_str(arguments);
                            yield StreamEvent::ToolCallDelta {
                                index: Some(index),
                                id: id.into(),
                                name: name.map(str::to_owned),
                                args_delta: arguments.into(),
                            };
                        }
                    }
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        let reason = match reason {
                            "stop" => FinishReason::Stop,
                            "tool_calls" => FinishReason::ToolUse,
                            "length" => FinishReason::MaxTokens,
                            "content_filter" => FinishReason::ContentFilter,
                            "error" => FinishReason::Error,
                            _ => Err(protocol("unknown completion disposition"))?,
                        };
                        if finished.as_ref().is_some_and(|old| old != &reason) {
                            Err(protocol("conflicting terminal finish"))?;
                        }
                        finished = Some(reason);
                    }
                }
            }
            if eof {
                break;
            }
        }
        if !pending_reasoning.is_empty() {
            yield StreamEvent::ReasoningDelta {
                delta: std::mem::take(&mut pending_reasoning),
            };
        }
        if !sentinel
            || generation.is_none()
            || !observed_provider
            || terminal_usage.is_none()
            || terminal_cost.is_none()
        {
            Err(protocol(
                "terminal route, usage, cost or SSE sentinel is missing",
            ))?;
        }
        let reasoning_tokens = terminal_usage
            .as_ref()
            .and_then(|usage| usage.usage.reasoning_tokens)
            .unwrap_or(0);
        let mut replay = reasoning.encoded()?;
        if replay
            .as_ref()
            .is_some_and(|encoded| encoded.len() > bounds.response_bytes)
        {
            Err(protocol("reasoning details exceed the retained response bound"))?;
        }
        let mut ids = std::collections::HashSet::new();
        for (index, call) in calls {
            axocoatl_llm::validate_required_tool_call_id(PROVIDER, &call.id)?;
            if !ids.insert(call.id.clone()) {
                Err(protocol("duplicate tool identity"))?;
            }
            let arguments: Value = serde_json::from_str(&call.args)
                .map_err(|_| protocol("incomplete native tool arguments"))?;
            validate_response_tool_call(PROVIDER, &call.name, &arguments, &request.tools)?;
            let mut metadata = provider_tool_metadata(PROVIDER);
            metadata.insert(
                "axocoatl.openrouter.endpoint".into(),
                profile.endpoint_tag.clone(),
            );
            metadata.insert(
                "axocoatl.openrouter.generation".into(),
                generation.clone().unwrap(),
            );
            // The turn's reasoning goes back with its tool results; it rides
            // on the first call so the assistant message carries it once.
            if let Some(details) = replay.take() {
                metadata.insert(REASONING_DETAILS_METADATA.into(), details);
                metadata.insert(
                    REASONING_TOKENS_METADATA.into(),
                    reasoning_tokens.to_string(),
                );
            }
            yield StreamEvent::ToolCallMetadata {
                index: Some(index),
                id: call.id,
                metadata,
            };
        }
        yield StreamEvent::Done {
            finish_reason: finished.ok_or_else(|| protocol("terminal finish missing"))?,
        };
    })
}
pub(super) async fn collect(
    provider: &NativeOpenRouterProvider,
    request: ChatRequest,
) -> AccountedChatOutcome {
    let mut usage = MeasuredTokenUsage::lower_bound(TokenUsageStats::default());
    let mut cost_microunits = None;
    let response = async {
        let mut stream = provider.chat_stream(request).await?;
        let mut content = String::new();
        let mut calls = BTreeMap::<usize, ToolCall>::new();
        let mut raw = BTreeMap::<usize, String>::new();
        let mut finish = None;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta { delta } => content.push_str(&delta),
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                } => {
                    let index = index.ok_or_else(|| protocol("missing collected tool index"))?;
                    let call = calls.entry(index).or_insert_with(|| ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: json!({}),
                        provider_metadata: Default::default(),
                    });
                    if !id.is_empty() {
                        call.id = id;
                    }
                    if let Some(name) = name {
                        call.name = name;
                    }
                    raw.entry(index).or_default().push_str(&args_delta);
                }
                StreamEvent::ToolCallMetadata {
                    index, metadata, ..
                } => {
                    let call = calls
                        .get_mut(&index.ok_or_else(|| protocol("missing tool metadata index"))?)
                        .ok_or_else(|| protocol("unbound tool metadata"))?;
                    call.provider_metadata.extend(metadata);
                }
                StreamEvent::UsageObservation(observed) => usage = observed,
                StreamEvent::CostObservation {
                    cost_microunits: cost,
                } => cost_microunits = Some(cost),
                StreamEvent::Done { finish_reason } => {
                    finish = Some(finish_reason);
                    break;
                }
                _ => {}
            }
        }
        let mut tool_calls = Vec::new();
        for (index, mut call) in calls {
            call.arguments = serde_json::from_str(
                raw.get(&index)
                    .ok_or_else(|| protocol("missing collected arguments"))?,
            )
            .map_err(|_| protocol("invalid collected arguments"))?;
            tool_calls.push(call);
        }
        let response = ChatResponse {
            content,
            tool_calls,
            finish_reason: finish.ok_or_else(|| protocol("missing collected completion"))?,
            usage: usage.usage.clone(),
            model: provider.model_id().into(),
            provider: PROVIDER.into(),
        };
        validate_chat_response(PROVIDER, &response)?;
        Ok(response)
    }
    .await;
    if response.is_err() {
        usage.complete = false;
    }
    AccountedChatOutcome {
        response,
        usage,
        cost_microunits,
    }
}
