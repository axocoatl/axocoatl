use super::*;

fn usage(value: &Value) -> Result<MeasuredTokenUsage, ProviderError> {
    let input = value["prompt_tokens"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| protocol("terminal prompt usage missing"))?;
    let completion = value["completion_tokens"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| protocol("terminal completion usage missing"))?;
    let reasoning = value
        .get("completion_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .map(|n| {
            n.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| protocol("reasoning usage malformed"))
        })
        .transpose()?
        .unwrap_or(0);
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
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

pub(super) fn decode(
    response: reqwest::Response,
    request: ChatRequest,
    profile: NativeOpenRouterObservation,
    limit: usize,
    max_output_tokens: usize,
    key: String,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let bounds = profile.execution_bounds(max_output_tokens, limit)?;
        let output_limit = request.max_tokens.unwrap_or(max_output_tokens);
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::new(limit, limit);
        let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
        let mut finished = None;
        let mut terminal_usage = None;
        let mut terminal_cost = None;
        let mut calls = BTreeMap::<usize, Call>::new();
        let mut generation = None;
        let mut observed_provider = false;
        let mut sentinel = false;
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
                    if observed.usage.input_tokens > profile.context_tokens
                        || observed.usage.output_tokens > output_limit
                        || cost > bounds.cost_microunits
                    {
                        Err(protocol(
                            "provider usage exceeded the admitted execution contract",
                        ))?;
                    }
                    if observed.usage.reasoning_tokens.unwrap_or(0) != 0 {
                        Err(protocol("non-reasoning endpoint reported reasoning spend"))?;
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
                    if delta
                        .get("reasoning")
                        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
                        || delta
                            .get("reasoning_details")
                            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty()))
                    {
                        Err(protocol("unapproved reasoning response"))?;
                    }
                    if let Some(text) = delta.get("content").filter(|v| !v.is_null()) {
                        let text = text
                            .as_str()
                            .ok_or_else(|| protocol("nontext output unsupported"))?;
                        if finished.is_some() && !text.is_empty() {
                            Err(protocol("content after terminal completion"))?;
                        }
                        if !text.is_empty() {
                            yield StreamEvent::TextDelta { delta: text.into() };
                        }
                    }
                    if let Some(parts) = delta.get("tool_calls").filter(|v| !v.is_null()) {
                        if finished.is_some() {
                            Err(protocol("tool delta after terminal completion"))?;
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
