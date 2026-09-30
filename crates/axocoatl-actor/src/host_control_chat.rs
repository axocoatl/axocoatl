//! Existing Coordinator provider loop extended by its host-bound control tool.
use crate::error::AgentError;
use crate::{
    default_behavior::attach_to_last_user_message,
    execution_boundary::{
        dispatch_acknowledged, InvocationBackend, PendingToolInvocation, ToolInvocationRequest,
    },
    provider_budget::{self, ControlledChat},
    AgentRunControl,
};
use axocoatl_core::{ChatMessage, MessageRole, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_memory::SessionMemory;
use axocoatl_token::{TokenCounter, TokenTracker};
use axocoatl_tools::BuiltinTool;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Guidance is retained in the per-run history and, for ActorSession runs, the
/// exact checkpoint transcript before the host acknowledgement. Decomposition
/// must not close admission: only the final synthesis owns the terminal poll.
pub(crate) struct GuidanceTranscript<'a> {
    pub history: &'a mut Vec<ChatMessage>,
    pub session: Option<&'a mut SessionMemory>,
    pub final_boundary: bool,
}

impl GuidanceTranscript<'_> {
    fn append(&mut self, message: ChatMessage, counter: &dyn TokenCounter) {
        if let Some(session) = self.session.as_deref_mut() {
            session.append(
                message.role.clone(),
                message.text_content().unwrap_or_default(),
                counter.count_messages(std::slice::from_ref(&message)),
            );
            if message.role == MessageRole::User {
                session.replace_last_user_content(
                    &message.content,
                    counter.count_messages(std::slice::from_ref(&message)),
                );
            }
        }
        self.history.push(message);
    }

    fn consume(
        &mut self,
        request: &mut ChatRequest,
        counter: &dyn TokenCounter,
        control: Option<&AgentRunControl>,
        final_response: Option<&str>,
    ) -> Result<bool, AgentError> {
        let Some(control) = control else {
            return Ok(false);
        };
        let Some(boundary) = control.execution_boundary() else {
            return Ok(false);
        };
        let mut consumed = false;
        while !control.is_cancelled() {
            let delivery = boundary
                .take_guidance(self.final_boundary && final_response.is_some() && !consumed)
                .map_err(|reason| {
                    control.fail_execution_boundary(reason.clone());
                    AgentError::Internal(reason)
                })?;
            let Some(delivery) = delivery else {
                return Ok(consumed);
            };
            if !consumed {
                if let Some(response) = final_response {
                    let message = ChatMessage::assistant(response);
                    self.append(message.clone(), counter);
                    request.messages.push(message);
                }
            }
            request.messages.push(ChatMessage::user(&delivery.text));
            if !delivery.attachments.is_empty() {
                attach_to_last_user_message(request, &delivery.attachments);
            }
            let message = request.messages.last().expect("appended guidance").clone();
            self.append(message, counter);
            // The host has durably offered the exact handoff. No await or
            // fallible operation separates input append and its durable ack.
            delivery.acknowledgement.acknowledge().map_err(|reason| {
                control.fail_execution_boundary(reason.clone());
                AgentError::Internal(reason)
            })?;
            consumed = true;
        }
        Ok(consumed)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_with_tools(
    provider: &dyn LlmProvider,
    counter: &dyn TokenCounter,
    tracker: Option<&TokenTracker>,
    usage: Option<&crate::behavior::ExecutionUsageState>,
    mut request: ChatRequest,
    protected: usize,
    control: Option<&AgentRunControl>,
    tools: &[(String, Arc<dyn BuiltinTool>)],
    actor_id: &str,
    sequence: &AtomicU64,
    mut transcript: GuidanceTranscript<'_>,
) -> Result<ControlledChat, AgentError> {
    let boundary = control
        .and_then(AgentRunControl::execution_boundary)
        .cloned();
    if !tools.is_empty() && boundary.is_none() {
        return Err(AgentError::Internal(
            "scoped control tool lacks host boundary".into(),
        ));
    }
    let mut total = TokenUsageStats::default();
    loop {
        transcript.consume(&mut request, counter, control, None)?;
        if control.is_some_and(AgentRunControl::is_cancelled) {
            return Ok(ControlledChat::Cancelled);
        }
        request.tools = tools
            .iter()
            .filter_map(|(name, tool)| {
                tool.advertised_parameters_schema()
                    .map(|parameters| axocoatl_llm::ToolDefinition {
                        name: name.clone(),
                        description: tool.description().into(),
                        parameters,
                        concurrency: tool.concurrency_policy(),
                    })
            })
            .collect();
        let mut response = match provider_budget::chat(
            provider,
            counter,
            tracker,
            usage,
            request.clone(),
            protected,
            control,
        )
        .await?
        {
            ControlledChat::Response(response) => response,
            ControlledChat::Cancelled => return Ok(ControlledChat::Cancelled),
        };
        total.merge(&response.usage);
        if response.tool_calls.is_empty() {
            let consumed =
                transcript.consume(&mut request, counter, control, Some(&response.content))?;
            if control.is_some_and(AgentRunControl::is_cancelled) {
                return Ok(ControlledChat::Cancelled);
            }
            if consumed {
                continue;
            }
            response.usage = total;
            return Ok(ControlledChat::Response(response));
        }
        if response.tool_calls.len() > 128
            || response
                .tool_calls
                .iter()
                .any(|call| !tools.iter().any(|(name, _)| name == &call.name))
        {
            return Err(AgentError::Internal(
                "Coordinator returned a tool outside the exact host control port".into(),
            ));
        }
        let group = sequence.fetch_add(1, Ordering::SeqCst);
        let calls = response
            .tool_calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let tool = &tools
                    .iter()
                    .find(|(name, _)| name == &call.name)
                    .expect("validated host tool")
                    .1;
                PendingToolInvocation {
                    request: ToolInvocationRequest {
                        actor_id: actor_id.into(),
                        provider_id: provider.provider_id().into(),
                        model_id: provider.model_id().into(),
                        provider_response_group: group,
                        provider_call_index: index,
                        provider_call_count: response.tool_calls.len(),
                        tool_call: call.clone(),
                    },
                    backend: InvocationBackend::Behavior(tool.clone()),
                    policy: tool.concurrency_policy(),
                }
            })
            .collect();
        let results = dispatch_acknowledged(
            calls,
            boundary
                .clone()
                .ok_or_else(|| AgentError::Internal("host tool lacks boundary".into()))?,
            control
                .cloned()
                .ok_or_else(|| AgentError::Internal("host tool lacks run control".into()))?,
        )
        .await
        .map_err(AgentError::Internal)?;
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                &response.content,
                response.tool_calls,
            ));
        for result in results {
            let value = match result.result.result {
                Ok(value) => value,
                Err(error) => serde_json::json!({"error":error.to_string()}),
            };
            request.messages.push(ChatMessage::tool_result(
                value.to_string(),
                result.result.tool_call.name,
                result.result.tool_call.id,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Mutex};

    struct Counter;
    impl TokenCounter for Counter {
        fn count_text(&self, text: &str) -> usize {
            text.len()
        }
        fn count_messages(&self, messages: &[ChatMessage]) -> usize {
            messages
                .iter()
                .map(|message| message.text_content().unwrap_or_default().len())
                .sum()
        }
        fn count_tool_definition(&self, _: &serde_json::Value) -> usize {
            0
        }
    }
    struct Ack {
        count: Arc<AtomicUsize>,
        fail: bool,
    }
    impl crate::SteeringAcknowledgement for Ack {
        fn acknowledge(self: Box<Self>) -> Result<(), String> {
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("durable guidance acknowledgement failed".into())
            } else {
                Ok(())
            }
        }
    }
    struct Boundary {
        polls: Mutex<Vec<bool>>,
        delivery: Mutex<Option<crate::SteeringDelivery>>,
    }
    #[async_trait::async_trait]
    impl crate::ToolExecutionBoundary for Boundary {
        async fn admit(
            &self,
            _: &ToolInvocationRequest,
        ) -> Result<Box<dyn crate::AdmittedToolInvocation>, String> {
            Err("this fixture cannot dispatch tools".into())
        }
        fn take_guidance(
            &self,
            final_boundary: bool,
        ) -> Result<Option<crate::SteeringDelivery>, String> {
            self.polls.lock().unwrap().push(final_boundary);
            if final_boundary {
                Ok(self.delivery.lock().unwrap().take())
            } else {
                Ok(None)
            }
        }
    }
    fn boundary(fail: bool) -> (Arc<Boundary>, Arc<AtomicUsize>, AgentRunControl) {
        let count = Arc::new(AtomicUsize::new(0));
        let boundary = Arc::new(Boundary {
            polls: Mutex::new(vec![]),
            delivery: Mutex::new(Some(crate::SteeringDelivery {
                text: "Check the retained finding".into(),
                attachments: vec![axocoatl_core::AgentAttachment {
                    id: "finding-attachment".into(),
                    name: "finding.txt".into(),
                    mime: "text/plain".into(),
                    bytes: vec![],
                    size: 18,
                    extracted_text: Some("exact source bytes".into()),
                }],
                acknowledgement: Box::new(Ack {
                    count: count.clone(),
                    fail,
                }),
            })),
        });
        let control = AgentRunControl::new(crate::AgentRunId::new("coordinator"))
            .with_execution_boundary(boundary.clone());
        (boundary, count, control)
    }

    #[test]
    fn synthesis_guidance_retains_answer_and_attachment_before_final_close() {
        let (boundary, count, control) = boundary(false);
        let mut request = ChatRequest::simple("goal");
        let mut history = request.messages.clone();
        let mut session = SessionMemory::new();
        session.append(MessageRole::User, "goal", 4);
        let mut transcript = GuidanceTranscript {
            history: &mut history,
            session: Some(&mut session),
            final_boundary: true,
        };
        assert!(transcript
            .consume(
                &mut request,
                &Counter,
                Some(&control),
                Some("completed answer")
            )
            .unwrap());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(request.messages[1].text_content(), Some("completed answer"));
        assert!(serde_json::to_string(&request.messages[2].content)
            .unwrap()
            .contains("exact source bytes"));
        assert_eq!(
            serde_json::to_value(&request.messages).unwrap(),
            serde_json::to_value(&*transcript.history).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&request.messages).unwrap(),
            serde_json::to_value(transcript.session.as_ref().unwrap().as_chat_messages()).unwrap()
        );
        // A consumed final handoff must not close admission before its successor
        // response. Only the later empty final poll does that.
        assert!(!transcript
            .consume(
                &mut request,
                &Counter,
                Some(&control),
                Some("revised answer")
            )
            .unwrap());
        assert_eq!(*boundary.polls.lock().unwrap(), vec![true, false, true]);
    }

    #[test]
    fn decomposition_does_not_close_steering_admission() {
        let (boundary, count, control) = boundary(false);
        let mut request = ChatRequest::simple("goal");
        let mut history = vec![];
        let mut transcript = GuidanceTranscript {
            history: &mut history,
            session: None,
            final_boundary: false,
        };
        assert!(!transcript
            .consume(&mut request, &Counter, Some(&control), Some("subtasks"))
            .unwrap());
        assert_eq!(*boundary.polls.lock().unwrap(), vec![false]);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(history.is_empty());
    }

    #[test]
    fn failed_guidance_acknowledgement_fails_boundary_after_retaining_input() {
        let (_, count, control) = boundary(true);
        let mut request = ChatRequest::simple("goal");
        let mut history = vec![];
        let mut transcript = GuidanceTranscript {
            history: &mut history,
            session: None,
            final_boundary: true,
        };
        let error = transcript
            .consume(
                &mut request,
                &Counter,
                Some(&control),
                Some("completed answer"),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("durable guidance acknowledgement failed"));
        assert!(control.execution_boundary_failure().is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(history.len(), 2);
        assert!(serde_json::to_string(&history[1].content)
            .unwrap()
            .contains("Check the retained finding"));
    }
}
