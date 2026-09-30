//! Acknowledged actor observations under the canonical controller mutex.
//! Persistence precedes publication; no forwarding queue owns unrecorded output.
use super::*;
use axocoatl_actor::{AgentStreamChunk, AgentStreamObserver};
use axocoatl_session::execution_content::{ActivationStreamContent, ActivationStreamPayload};
use std::io::{self, Write};

pub(super) struct ActivationStreamObserver {
    controller: SessionDispatchController,
    activation: ActivationRef,
    sequence: Mutex<StreamSequence>,
}

#[derive(Default)]
struct StreamSequence {
    next: u64,
    tools: crate::stream::ToolCallOccurrences,
}

impl ActivationStreamObserver {
    pub(super) fn new(controller: SessionDispatchController, activation: ActivationRef) -> Self {
        Self {
            controller,
            activation,
            sequence: Mutex::new(StreamSequence::default()),
        }
    }
}

impl SessionDispatchController {
    #[cfg(test)]
    pub(crate) fn stream_observer_for_test(
        &self,
        activation: ActivationRef,
    ) -> Arc<dyn AgentStreamObserver> {
        Arc::new(ActivationStreamObserver::new(self.clone(), activation))
    }

    /// Host-only publication join. A reconnect subscribes to the same existing
    /// bus and reads canonical evidence; it does not create another producer.
    pub(crate) fn attach_stream_bus(&self, bus: crate::stream::StreamBus) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        // Recovered closed turns retain their Stop fence. This host publication
        // join grants no execution and must work before a separately admitted
        // successor; lifecycle retirement still refuses new attachments.
        if state.execution_admission_closed {
            return Err(error("Session lifecycle has closed stream attachment"));
        }
        if state.stream_bus.is_some() {
            return Err(error("this controller already has a stream bus"));
        }
        state.stream_bus = Some(bus);
        Ok(())
    }
}

impl AgentStreamObserver for ActivationStreamObserver {
    fn observe(&self, chunk: &AgentStreamChunk) -> std::result::Result<(), String> {
        self.record(chunk).map_err(|error| error.to_string())
    }
}

impl ActivationStreamObserver {
    fn record(&self, chunk: &AgentStreamChunk) -> Result<()> {
        let payload = match chunk {
            AgentStreamChunk::Text(delta) => ActivationStreamPayload::Text {
                delta: delta.clone(),
            },
            AgentStreamChunk::ProviderRetry { reason } => ActivationStreamPayload::ProviderRetry {
                reason: reason.chars().take(512).collect(),
            },
            AgentStreamChunk::Reasoning(delta) => ActivationStreamPayload::ReasoningSummary {
                delta: delta.clone(),
            },
            AgentStreamChunk::ToolCallStarted {
                source_agent,
                id,
                name,
                arguments,
                provider_response_group,
                provider_call_index,
                provider_call_count,
                ..
            } => {
                if source_agent.is_some() {
                    return Err(error(
                        "child stream requires its own exact activation observer",
                    ));
                }
                let (arguments_sha256, arguments_bytes) = observation_identity(arguments)?;
                ActivationStreamPayload::ToolProposed {
                    call_id: id.clone(),
                    name: name.clone(),
                    provider_response_group: *provider_response_group,
                    provider_call_index: *provider_call_index,
                    provider_call_count: *provider_call_count,
                    arguments_sha256,
                    arguments_bytes,
                }
            }
            AgentStreamChunk::ToolCallResult {
                source_agent,
                id,
                name,
                result,
                is_error,
            } => {
                if source_agent.is_some() {
                    return Err(error(
                        "child stream requires its own exact activation observer",
                    ));
                }
                let (result_sha256, result_bytes) = observation_identity(result)?;
                ActivationStreamPayload::ToolResult {
                    call_id: id.clone(),
                    name: name.clone(),
                    result_sha256,
                    result_bytes,
                    is_error: *is_error,
                }
            }
        };
        // Lock order is observer -> canonical. Neither lock spans an await or
        // invokes a provider/tool/client callback. Stop may wait for the bounded
        // synchronous persistence operation under the canonical lock.
        let mut sequence = self
            .sequence
            .lock()
            .map_err(|_| error("stream sequence lock failed"))?;
        let mut state = self.controller.lock()?;
        state.ready()?;
        let snapshot = state.current(&self.activation)?;
        if !state
            .bound
            .get(&self.activation.activation_id)
            .is_some_and(|bound| bound.activation == self.activation)
        {
            return Err(error("stream producer has no exact bound activation"));
        }
        let content = ActivationStreamContent {
            schema_version: 1,
            activation: self.activation.clone(),
            sequence: sequence.next,
            recorded_at_unix_ms: now_ms()?,
            payload,
        };
        #[cfg(test)]
        let admission = state.trip(TestFailure::StreamObservation);
        #[cfg(not(test))]
        let admission: Result<()> = Ok(());
        let retained = admission.and_then(|()| {
            state
                .content
                .record_activation_stream(&snapshot, content)
                .map_err(error)
        });
        let event = state.fail_closed(retained)?;
        sequence.next = sequence
            .next
            .checked_add(1)
            .ok_or_else(|| error("stream sequence exhausted"))?;
        // The existing Ways display uses occurrence identities to pair reused
        // provider call IDs. Allocate them under the same observer lock only
        // after the canonical stream observation has been retained.
        let tool_occurrence = match chunk {
            AgentStreamChunk::ToolCallStarted { id, .. } => Some(sequence.tools.start(id)),
            AgentStreamChunk::ToolCallResult { id, .. } => Some(sequence.tools.finish(id)),
            _ => None,
        };
        // Sending without listeners is harmless: reconnect reads the exact
        // persisted content. Lagging subscribers detect the bus cursor gap.
        if let Some(bus) = &state.stream_bus {
            if let Some((_, admission)) = state
                .content
                .turn_admission(&state.canonical, &state.turn_id)
                .map_err(error)?
            {
                if let Ok(ways) = serde_json::from_str::<
                    crate::bootstrap::native_ways::NativeWaysAdmission,
                >(&admission.source)
                {
                    if let Some(candidate) = ways
                        .candidates
                        .iter()
                        .find(|candidate| candidate.activation == self.activation)
                    {
                        let _ = bus.send(crate::bootstrap::agent_chunk_stream_frame(
                            chunk,
                            &candidate.run_id,
                            candidate.definition.definition_id.as_str(),
                            Some(self.activation.turn_id.as_str()),
                            tool_occurrence,
                            Some(self.activation.generation),
                        ));
                    }
                }
            }
            let _ = bus.send(crate::stream::StreamFrame::ActivationStream { event });
        }
        Ok(())
    }
}

fn observation_identity(value: &serde_json::Value) -> Result<(String, u64)> {
    struct DigestWriter {
        digest: Sha256,
        bytes: usize,
    }
    impl Write for DigestWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_RESULT_BYTES.saturating_sub(self.bytes) {
                return Err(io::Error::other(
                    "actor observation exceeds its representation bound",
                ));
            }
            self.digest.update(bytes);
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = DigestWriter {
        digest: Sha256::new(),
        bytes: 0,
    };
    serde_json::to_writer(&mut writer, value).map_err(error)?;
    Ok((hex::encode(writer.digest.finalize()), writer.bytes as u64))
}
