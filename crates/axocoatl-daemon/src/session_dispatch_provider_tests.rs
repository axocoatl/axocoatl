use super::*;
use crate::session_dispatch::provider::SessionProvider;
use axocoatl_core::MeasuredTokenUsage;
use axocoatl_llm::{AccountedChatOutcome, ProviderExecutionBounds};
use axocoatl_session::control_authority::ProviderUsage;
use tokio_stream::StreamExt;

struct BoundedProvider {
    calls: AtomicUsize,
    bounds: Option<ProviderExecutionBounds>,
    events: Mutex<Option<Vec<std::result::Result<StreamEvent, ProviderError>>>>,
    pending_tail: bool,
    pending_open: bool,
    entered: tokio::sync::Notify,
    response: Option<ChatResponse>,
    accounting: Option<MeasuredTokenUsage>,
    cost_microunits: Option<u64>,
    /// A follow-up call reserves more as its prompt grows, one token and one
    /// microunit per added prompt token, as a native OpenRouter call does.
    grows: bool,
}

impl BoundedProvider {
    fn new(events: Vec<std::result::Result<StreamEvent, ProviderError>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            bounds: Some(ProviderExecutionBounds {
                token_limit: 100,
                cost_microunits: 10,
                response_bytes: 4096,
            }),
            events: Mutex::new(Some(events)),
            pending_tail: false,
            pending_open: false,
            entered: tokio::sync::Notify::new(),
            response: None,
            accounting: None,
            cost_microunits: None,
            grows: false,
        }
    }
}

#[async_trait]
impl LlmProvider for BoundedProvider {
    fn provider_id(&self) -> &str {
        "controlled"
    }
    fn model_id(&self) -> &str {
        "controlled-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        self.bounds
    }
    fn follow_up_execution_bounds(
        &self,
        _: &ChatRequest,
        added_prompt_tokens: u64,
    ) -> Option<ProviderExecutionBounds> {
        let mut bounds = self.bounds?;
        if self.grows {
            bounds.token_limit += added_prompt_tokens;
            bounds.cost_microunits += added_prompt_tokens;
        }
        Some(bounds)
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.response
            .clone()
            .ok_or_else(|| ProviderError::Network("no terminal response".into()))
    }
    async fn chat_with_accounting(&self, request: ChatRequest) -> AccountedChatOutcome {
        let response = self.chat(request).await;
        let usage = self.accounting.clone().unwrap_or_else(|| {
            MeasuredTokenUsage::lower_bound(
                response
                    .as_ref()
                    .map(|response| response.usage.clone())
                    .unwrap_or_default(),
            )
        });
        AccountedChatOutcome {
            response,
            usage,
            cost_microunits: self.cost_microunits,
        }
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.pending_open {
            std::future::pending::<()>().await;
        }
        let events = self.events.lock().unwrap().take().unwrap_or_default();
        let stream = tokio_stream::iter(events);
        if self.pending_tail {
            Ok(Box::pin(stream.chain(tokio_stream::pending())))
        } else {
            Ok(Box::pin(stream))
        }
    }
}

#[tokio::test]
async fn provider_cancelled_open_future_retains_unknown_incurred_usage() {
    let mut inner = BoundedProvider::new(vec![]);
    inner.pending_open = true;
    let inner = Arc::new(inner);
    let (fixture, wrapped) = provider_fixture(inner.clone());
    let mut opening = Box::pin(wrapped.chat_stream(ChatRequest::simple("input")));
    tokio::select! {
        _ = inner.entered.notified() => {},
        _ = &mut opening => panic!("provider opening must remain pending"),
    }
    assert_eq!(provider_usage(&fixture).unsettled_calls, 1);
    drop(opening);
    let usage = provider_usage(&fixture);
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(!usage.tokens.complete);
    assert_eq!(usage.tokens.usage, TokenUsageStats::default());
}

fn provider_fixture(inner: Arc<BoundedProvider>) -> (Fixture, SessionProvider) {
    provider_fixture_with_reservations(inner, true, true)
}

fn provider_fixture_with_reservations(
    inner: Arc<BoundedProvider>,
    checkpoint: bool,
    output: bool,
) -> (Fixture, SessionProvider) {
    let fixture = fixture_with_limits(
        GrantLimits {
            activations: 8,
            invocations: 32,
            tokens: 1000,
            cost_microunits: 100,
        },
        "in-process-test",
    );
    {
        let mut state = fixture.controller.lock().unwrap();
        let bound = state
            .bind_with_provider_gate(
                fixture.activation.clone(),
                fixture.profile.clone(),
                &serde_json::to_string(&fixture.config).unwrap(),
                AgentRunControl::new(axocoatl_actor::AgentRunId::new("activation")),
                true,
                now_ms().unwrap(),
            )
            .unwrap();
        state
            .bound
            .insert(fixture.activation.activation_id.clone(), bound);
        let snapshot = state.current(&fixture.activation).unwrap();
        let DispatchState {
            memory,
            content,
            canonical,
            ..
        } = &mut *state;
        if checkpoint {
            memory
                .reserve_candidate(canonical, &fixture.activation)
                .unwrap();
        }
        if output {
            content
                .reserve_activation_output(
                    &snapshot,
                    &fixture.activation,
                    axocoatl_session::execution_content::ActivationOutputLimits {
                        partial_records: 0,
                        partial_bytes: 0,
                        settlement_bytes: 1024 * 1024,
                    },
                )
                .unwrap();
        }
    }
    let wrapped = SessionProvider::new(
        fixture.controller.clone(),
        fixture.activation.clone(),
        inner,
        fixture.profile.provider.clone(),
        fixture.profile.model.clone(),
    );
    (fixture, wrapped)
}

#[tokio::test]
async fn provider_requires_both_physical_settlement_reservations_before_dispatch() {
    for (checkpoint, output) in [(false, false), (true, false), (false, true)] {
        let inner = Arc::new(BoundedProvider::new(vec![]));
        let (fixture, wrapped) =
            provider_fixture_with_reservations(inner.clone(), checkpoint, output);
        assert!(wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider_usage(&fixture).calls, 0);
    }
}

fn provider_usage(fixture: &Fixture) -> ProviderUsage {
    fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .provider_usage(&fixture.activation)
        .unwrap()
}

#[tokio::test]
async fn provider_done_is_delivered_only_after_complete_usage_is_durable() {
    let inner = Arc::new(BoundedProvider::new(vec![
        Ok(StreamEvent::TextDelta {
            delta: "answer".into(),
        }),
        Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ]));
    let (fixture, wrapped) = provider_fixture(inner.clone());
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    assert_eq!(provider_usage(&fixture).unsettled_calls, 1);
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::TextDelta { .. }))
    ));
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::Usage(_)))
    ));
    assert!(!provider_usage(&fixture).tokens.complete);
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::Done { .. }))
    ));
    let usage = provider_usage(&fixture);
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(usage.tokens.complete);
    assert_eq!(usage.tokens.usage, TokenUsageStats::new(10, 2));
    assert!(!usage.cost_known);
    assert!(stream.next().await.is_none());
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_drop_error_and_eof_keep_usage_incomplete_even_after_a_usage_event() {
    for ending in ["drop", "error", "eof"] {
        let mut events = vec![Ok(StreamEvent::Usage(TokenUsageStats::new(7, 3)))];
        if ending == "error" {
            events.push(Err(ProviderError::Network("lost body".into())));
        }
        let inner = Arc::new(BoundedProvider::new(events));
        let (fixture, wrapped) = provider_fixture(inner);
        let mut stream = wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamEvent::Usage(_)))
        ));
        if ending != "drop" {
            assert!(matches!(stream.next().await, Some(Err(_))), "{ending}");
        }
        drop(stream);
        let usage = provider_usage(&fixture);
        assert_eq!(usage.unsettled_calls, 0, "{ending}");
        assert!(!usage.tokens.complete, "{ending}");
        assert_eq!(usage.tokens.usage, TokenUsageStats::new(7, 3), "{ending}");
    }
}

#[tokio::test]
async fn provider_refuses_missing_bounds_route_mismatch_oversized_bound_and_stop_before_dispatch() {
    for fault in ["missing", "model", "oversized", "stop"] {
        let mut inner = BoundedProvider::new(vec![]);
        if fault == "missing" {
            inner.bounds = None;
        }
        if fault == "oversized" {
            inner.bounds.as_mut().unwrap().response_bytes = 1024 * 1024 + 1;
        }
        let inner = Arc::new(inner);
        let (fixture, wrapped) = provider_fixture(inner.clone());
        let mut request = ChatRequest::simple("input");
        if fault == "model" {
            request.model_override = Some("foreign-model".into());
        }
        if fault == "stop" {
            fixture
                .controller
                .stop_activation(&fixture.activation)
                .unwrap();
        }
        assert!(wrapped.chat_stream(request).await.is_err(), "{fault}");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0, "{fault}");
        assert_eq!(provider_usage(&fixture).calls, 0, "{fault}");
    }
}

#[tokio::test]
async fn provider_caps_reasoning_native_metadata_and_cumulative_stream_bytes() {
    for field in ["text", "reasoning", "arguments", "metadata", "cumulative"] {
        let long = "x".repeat(512);
        let events = match field {
            "text" => vec![Ok(StreamEvent::TextDelta { delta: long })],
            "reasoning" => vec![Ok(StreamEvent::ReasoningDelta { delta: long })],
            "arguments" => vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: "call".into(),
                name: Some("effect".into()),
                args_delta: long,
            })],
            "metadata" => vec![Ok(StreamEvent::ToolCallMetadata {
                index: Some(0),
                id: "call".into(),
                metadata: axocoatl_core::ProviderMetadata::from([("native".into(), long)]),
            })],
            _ => (0..8)
                .map(|_| {
                    Ok(StreamEvent::TextDelta {
                        delta: "x".repeat(48),
                    })
                })
                .collect(),
        };
        let mut inner = BoundedProvider::new(events);
        inner.bounds.as_mut().unwrap().response_bytes = 256;
        let (fixture, wrapped) = provider_fixture(Arc::new(inner));
        let mut stream = wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .unwrap();
        let mut failed = false;
        while let Some(event) = stream.next().await {
            if event.is_err() {
                failed = true;
                break;
            }
            assert!(!matches!(event, Ok(StreamEvent::Done { .. })));
        }
        assert!(failed, "{field}");
        assert!(
            fixture.controller.lock().unwrap().poisoned.is_some(),
            "{field}"
        );
        assert!(!provider_usage(&fixture).tokens.complete, "{field}");
    }
}

#[tokio::test]
async fn provider_usage_overrun_and_decreasing_snapshots_are_retained_before_refusal() {
    for (overrun, explicit) in [(false, false), (true, false), (false, true), (true, true)] {
        let observation = |usage| {
            Ok(if explicit {
                StreamEvent::UsageObservation(MeasuredTokenUsage::lower_bound(usage))
            } else {
                StreamEvent::Usage(usage)
            })
        };
        let events = if overrun {
            vec![observation(TokenUsageStats::new(120, 2))]
        } else {
            vec![
                observation(TokenUsageStats::new(10, 5)),
                observation(TokenUsageStats::new(7, 8)),
            ]
        };
        let (fixture, wrapped) = provider_fixture(Arc::new(BoundedProvider::new(events)));
        let mut stream = wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .unwrap();
        if !overrun {
            assert!(stream.next().await.unwrap().is_ok());
        }
        assert!(stream.next().await.unwrap().is_err());
        let usage = provider_usage(&fixture);
        assert!(!usage.tokens.complete);
        assert_eq!(
            usage.tokens.usage,
            if overrun {
                TokenUsageStats::new(120, 2)
            } else {
                TokenUsageStats::new(10, 8)
            }
        );
        assert!(fixture.controller.lock().unwrap().poisoned.is_some());
    }
}

#[tokio::test]
async fn provider_nonstreaming_completion_and_failure_share_the_durable_boundary() {
    for known in [false, true] {
        let mut inner = BoundedProvider::new(vec![]);
        inner.response = Some(ChatResponse {
            content: "summary".into(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: if known {
                TokenUsageStats::new(8, 2)
            } else {
                TokenUsageStats::default()
            },
            model: "controlled-model".into(),
            provider: "controlled".into(),
        });
        if known {
            inner.accounting = Some(MeasuredTokenUsage::known(TokenUsageStats::new(8, 2)));
        }
        let (fixture, wrapped) = provider_fixture(Arc::new(inner));
        assert_eq!(
            wrapped
                .chat(ChatRequest::simple("summarize"))
                .await
                .unwrap()
                .content,
            "summary"
        );
        let usage = provider_usage(&fixture);
        assert_eq!(usage.unsettled_calls, 0);
        assert_eq!(usage.tokens.complete, known);
    }
    let (fixture, wrapped) = provider_fixture(Arc::new(BoundedProvider::new(vec![])));
    assert!(wrapped
        .chat(ChatRequest::simple("summarize"))
        .await
        .is_err());
    let usage = provider_usage(&fixture);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(!usage.tokens.complete);
}

#[tokio::test]
async fn provider_nonstream_accounting_preserves_explicit_zero_partial_and_failed_decode() {
    for case in [
        "known-zero",
        "partial",
        "decode-failed",
        "decode-failed-known",
    ] {
        let mut inner = BoundedProvider::new(vec![]);
        let observed = if case == "known-zero" {
            MeasuredTokenUsage::known(TokenUsageStats::default())
        } else if case == "decode-failed-known" {
            MeasuredTokenUsage::known(TokenUsageStats::new(9, 4))
        } else {
            MeasuredTokenUsage::lower_bound(TokenUsageStats::new(9, 0).with_reasoning(2))
        };
        let failed = case.starts_with("decode-failed");
        let mut expected = observed.clone();
        expected.complete &= !failed;
        inner.accounting = Some(observed.clone());
        if !failed {
            inner.response = Some(ChatResponse {
                content: "answer".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: observed.usage.clone(),
                model: "controlled-model".into(),
                provider: "controlled".into(),
            });
        }
        let inner = Arc::new(inner);
        let (fixture, wrapped) = provider_fixture(inner.clone());
        let returned = wrapped
            .chat_with_accounting(ChatRequest::simple("input"))
            .await;
        assert_eq!(returned.response.is_err(), failed, "{case}");
        assert_eq!(returned.usage, expected, "{case}");
        let usage = provider_usage(&fixture);
        assert_eq!(usage.tokens, expected, "{case}");
        assert_eq!(usage.calls, 1, "{case}");
        assert_eq!(usage.unsettled_calls, 0, "{case}");
        assert!(!usage.cost_known, "{case}");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1, "{case}");
    }
}

#[tokio::test]
async fn provider_conflicting_nonstream_observations_keep_highwater_before_refusal() {
    let mut inner = BoundedProvider::new(vec![]);
    inner.accounting = Some(MeasuredTokenUsage::known(TokenUsageStats::new(9, 2)));
    inner.response = Some(ChatResponse {
        content: "answer".into(),
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: TokenUsageStats::new(3, 5),
        model: "controlled-model".into(),
        provider: "controlled".into(),
    });
    let (fixture, wrapped) = provider_fixture(Arc::new(inner));
    let outcome = wrapped
        .chat_with_accounting(ChatRequest::simple("input"))
        .await;
    assert!(outcome.response.is_err());
    let expected = MeasuredTokenUsage::lower_bound(TokenUsageStats::new(9, 5));
    assert_eq!(outcome.usage, expected);
    assert_eq!(provider_usage(&fixture).tokens, expected);
    assert!(fixture.controller.lock().unwrap().poisoned.is_some());
}

#[tokio::test]
async fn provider_new_usage_snapshots_require_explicit_completeness_and_terminal_done() {
    for complete in [false, true] {
        for interrupted in [false, true] {
            let mut events = vec![
                Ok(StreamEvent::UsageObservation(
                    MeasuredTokenUsage::lower_bound(TokenUsageStats::new(3, 0)),
                )),
                Ok(StreamEvent::UsageObservation(MeasuredTokenUsage {
                    usage: TokenUsageStats::new(7, 2),
                    complete,
                })),
            ];
            if !interrupted {
                events.push(Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }));
            }
            let (fixture, wrapped) = provider_fixture(Arc::new(BoundedProvider::new(events)));
            let mut stream = wrapped
                .chat_stream(ChatRequest::simple("input"))
                .await
                .unwrap();
            assert!(stream.next().await.unwrap().is_ok());
            assert!(stream.next().await.unwrap().is_ok());
            if !interrupted {
                assert!(matches!(
                    stream.next().await,
                    Some(Ok(StreamEvent::Done { .. }))
                ));
            }
            drop(stream);
            let usage = provider_usage(&fixture);
            assert_eq!(usage.tokens.usage, TokenUsageStats::new(7, 2));
            assert_eq!(usage.tokens.complete, complete && !interrupted);
            assert_eq!(usage.unsettled_calls, 0);
        }
    }
}

#[tokio::test]
async fn provider_zero_api_charge_is_known_independently_of_token_completion_and_stop() {
    for ending in ["done", "drop", "error", "eof", "open-cancel"] {
        let mut events = vec![Ok(StreamEvent::UsageObservation(
            MeasuredTokenUsage::known(TokenUsageStats::new(4, 1)),
        ))];
        match ending {
            "done" => events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            })),
            "error" => events.push(Err(ProviderError::Network("lost body".into()))),
            _ => {}
        }
        let mut inner = BoundedProvider::new(events);
        inner.bounds.as_mut().unwrap().cost_microunits = 0;
        inner.pending_open = ending == "open-cancel";
        let inner = Arc::new(inner);
        let (fixture, wrapped) = provider_fixture(inner.clone());
        if ending == "open-cancel" {
            let mut opening = Box::pin(wrapped.chat_stream(ChatRequest::simple("input")));
            tokio::select! {
                _ = inner.entered.notified() => {},
                _ = &mut opening => panic!("provider opening must remain pending"),
            }
            let usage = provider_usage(&fixture);
            assert!(usage.cost_known);
            assert_eq!(usage.unsettled_calls, 1);
            assert!(!usage.tokens.complete);
            drop(opening);
        } else {
            let mut stream = wrapped
                .chat_stream(ChatRequest::simple("input"))
                .await
                .unwrap();
            assert!(stream.next().await.unwrap().is_ok());
            fixture
                .controller
                .stop_activation(&fixture.activation)
                .unwrap();
            if ending != "drop" {
                let terminal = stream.next().await.unwrap();
                assert_eq!(terminal.is_ok(), ending == "done", "{ending}");
            }
            drop(stream);
        }
        let usage = provider_usage(&fixture);
        assert!(usage.cost_known, "{ending}");
        assert_eq!(usage.cost_microunits, 0, "{ending}");
        assert_eq!(usage.tokens.complete, ending == "done", "{ending}");
        assert_eq!(
            usage.tokens.usage,
            if ending == "open-cancel" {
                TokenUsageStats::default()
            } else {
                TokenUsageStats::new(4, 1)
            },
            "{ending}"
        );
        assert_eq!(usage.calls, 1, "{ending}");
        assert_eq!(usage.unsettled_calls, 0, "{ending}");
        let state = fixture.controller.lock().unwrap();
        let charged = state.authority.usage("grant").unwrap();
        // Only a completed call settles to its 5 reported tokens, even after
        // Stop; every other ending keeps the whole reservation.
        assert_eq!(
            charged.tokens,
            if ending == "done" { 5 } else { 100 },
            "{ending}"
        );
        assert_eq!(charged.invocations, 1);
        assert_eq!(charged.cost_microunits, 0);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1, "{ending}");
    }
}

#[tokio::test]
async fn provider_usage_can_settle_after_stop_without_permitting_another_call() {
    let inner = Arc::new(BoundedProvider::new(vec![
        Ok(StreamEvent::Usage(TokenUsageStats::new(5, 1))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ]));
    let (fixture, wrapped) = provider_fixture(inner.clone());
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    fixture
        .controller
        .stop_activation(&fixture.activation)
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::Done { .. }))
    ));
    assert!(provider_usage(&fixture).tokens.complete);
    assert!(wrapped.chat(ChatRequest::simple("late")).await.is_err());
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_done_cannot_escape_when_the_owned_storage_root_was_replaced() {
    let (fixture, wrapped) = provider_fixture(Arc::new(BoundedProvider::new(vec![
        Ok(StreamEvent::Usage(TokenUsageStats::new(5, 1))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ])));
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    let path = fixture._root.path();
    let moved = path.with_extension("held-provider-root");
    std::fs::rename(path, &moved).unwrap();
    std::fs::create_dir(path).unwrap();
    let event = stream.next().await;
    std::fs::remove_dir(path).unwrap();
    std::fs::rename(&moved, path).unwrap();
    assert!(matches!(event, Some(Err(_))));
    assert!(fixture.controller.lock().unwrap().poisoned.is_some());
}

#[tokio::test]
async fn provider_authoritative_cost_settles_only_at_valid_terminal() {
    for complete in [false, true] {
        let mut events = vec![
            Ok(StreamEvent::Usage(TokenUsageStats::new(7, 3))),
            Ok(StreamEvent::CostObservation { cost_microunits: 4 }),
        ];
        if complete {
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }));
        }
        let inner = Arc::new(BoundedProvider::new(events));
        let (fixture, wrapped) = provider_fixture(inner.clone());
        let mut stream = wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        assert!(!provider_usage(&fixture).cost_known);
        if complete {
            assert!(matches!(
                stream.next().await,
                Some(Ok(StreamEvent::Done { .. }))
            ));
        } else {
            drop(stream);
        }
        let usage = provider_usage(&fixture);
        assert_eq!(usage.cost_microunits, 4);
        assert_eq!(usage.cost_known, complete);
        assert_eq!(usage.tokens.complete, complete);
        let charged = fixture
            .controller
            .lock()
            .unwrap()
            .authority
            .usage("grant")
            .unwrap();
        // A valid terminal settles the reservation to the measured tokens and
        // cost; without one the whole reservation stays charged.
        assert_eq!(charged.cost_microunits, if complete { 4 } else { 10 });
        assert_eq!(charged.tokens, if complete { 10 } else { 100 });
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn provider_cost_regression_and_overrun_retain_observed_charge_and_refuse_done() {
    for values in [[7, 4], [7, 12]] {
        let inner = Arc::new(BoundedProvider::new(vec![
            Ok(StreamEvent::Usage(TokenUsageStats::new(7, 3))),
            Ok(StreamEvent::CostObservation {
                cost_microunits: values[0],
            }),
            Ok(StreamEvent::CostObservation {
                cost_microunits: values[1],
            }),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }),
        ]));
        let (fixture, wrapped) = provider_fixture(inner.clone());
        let mut stream = wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        let usage = provider_usage(&fixture);
        assert_eq!(usage.cost_microunits, values[0].max(values[1]));
        assert!(!usage.cost_known);
        assert_eq!(usage.tokens.usage, TokenUsageStats::new(7, 3));
        assert!(wrapped
            .chat_stream(ChatRequest::simple("must not execute"))
            .await
            .is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn provider_nonstream_authoritative_cost_preserves_failure_subtotal() {
    for succeeds in [false, true] {
        let mut inner = BoundedProvider::new(vec![]);
        inner.accounting = Some(MeasuredTokenUsage::known(TokenUsageStats::new(7, 3)));
        inner.cost_microunits = Some(4);
        if succeeds {
            inner.response = Some(ChatResponse {
                content: "answer".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(7, 3),
                model: "controlled-model".into(),
                provider: "controlled".into(),
            });
        }
        let (fixture, wrapped) = provider_fixture(Arc::new(inner));
        let response = wrapped
            .chat_with_accounting(ChatRequest::simple("input"))
            .await;
        assert_eq!(response.response.is_ok(), succeeds);
        assert_eq!(response.cost_microunits, Some(4));
        let usage = provider_usage(&fixture);
        assert_eq!(usage.cost_microunits, 4);
        assert_eq!(usage.cost_known, succeeds);
        assert_eq!(usage.tokens.complete, succeeds);
    }
}

/// A helper's answer is read by a call that reserves more than any earlier
/// one: its prompt adds the lead's reasoning, the turn it returned and the
/// answer. Admission uses that estimate, not the largest earlier reservation.
#[tokio::test]
async fn a_tool_call_leaves_the_estimate_of_the_call_that_reads_its_result() {
    let tool_round = || {
        vec![
            Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: "call_1".into(),
                name: Some("effect".into()),
                args_delta: "{}".into(),
            }),
            Ok(StreamEvent::UsageObservation(MeasuredTokenUsage::known(
                TokenUsageStats {
                    input_tokens: 10,
                    output_tokens: 2,
                    reasoning_tokens: Some(40),
                },
            ))),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }),
        ]
    };
    let mut inner = BoundedProvider::new(tool_round());
    inner.grows = true;
    let (fixture, wrapped) = provider_fixture(Arc::new(inner));
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    let answer = crate::session_dispatch::delegate::ANSWER_PROMPT_TOKENS;
    let state = fixture.controller.lock().unwrap();
    let (activation, estimate) = state
        .follow_ups
        .get(&fixture.activation.activation_id)
        .cloned()
        .expect("a completed tool call leaves an estimate");
    assert_eq!(activation, fixture.activation);
    // 100 reserved, plus the 40 reasoning tokens sent back, the returned
    // turn's bytes and one helper answer.
    let growth = estimate.tokens - 100;
    assert!(growth > 40 + answer && growth < 40 + answer + 1024, "{growth}");
    assert_eq!(estimate.cost_microunits, 10 + growth);

    // The largest earlier reservation (100 tokens) would admit a helper that
    // leaves 848 of the 1,000; the follow-up needs far more.
    let bound = state.bound.get(&fixture.activation.activation_id).unwrap();
    let policy = state
        .authority
        .grant_policy(bound.grant.grant_id.as_str())
        .unwrap();
    let refused = state
        .delegate_follow_up_shortfall(
            &fixture.activation,
            &policy,
            "reader",
            &GrantLimits {
                activations: 1,
                invocations: 4,
                tokens: 100,
                cost_microunits: 0,
            },
        )
        .unwrap()
        .expect("the helper does not leave room to read its answer");
    assert!(
        refused.contains(&format!("{} tokens", estimate.tokens)),
        "{refused}"
    );
    drop(state);

    // A completion that asks for no tool leaves no estimate.
    let inner = BoundedProvider::new(vec![
        Ok(StreamEvent::TextDelta {
            delta: "answer".into(),
        }),
        Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ]);
    let (fixture, wrapped) = provider_fixture(Arc::new(inner));
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    assert!(fixture.controller.lock().unwrap().follow_ups.is_empty());
}

// ------------------------------------------------------------ retry policy

/// What a scripted executor does on each call, in order.
enum Scripted {
    /// `chat_stream` / `chat_with_accounting` fails before any response.
    Open(ProviderError),
    /// The stream yields these items.
    Events(Vec<std::result::Result<StreamEvent, ProviderError>>),
}

struct ScriptedProvider {
    calls: AtomicUsize,
    script: Mutex<std::collections::VecDeque<Scripted>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Scripted>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            script: Mutex::new(script.into()),
        })
    }
    fn next(&self) -> Scripted {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("an unscripted provider call")
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn provider_id(&self) -> &str {
        "controlled"
    }
    fn model_id(&self) -> &str {
        "controlled-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 10,
            response_bytes: 4096,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("the Session boundary uses chat_with_accounting")
    }
    async fn chat_with_accounting(&self, _: ChatRequest) -> AccountedChatOutcome {
        let response = match self.next() {
            Scripted::Open(error) => Err(error),
            Scripted::Events(_) => Ok(ChatResponse {
                content: "answer".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(10, 2),
                model: "controlled-model".into(),
                provider: "controlled".into(),
            }),
        };
        let usage = match &response {
            Ok(_) => MeasuredTokenUsage::known(TokenUsageStats::new(10, 2)),
            Err(_) => MeasuredTokenUsage::lower_bound(TokenUsageStats::default()),
        };
        AccountedChatOutcome {
            response,
            usage,
            cost_microunits: None,
        }
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        match self.next() {
            Scripted::Open(error) => Err(error),
            Scripted::Events(events) => Ok(Box::pin(tokio_stream::iter(events))),
        }
    }
}

/// A status error that names `Retry-After: 0 s`, so tests do not wait.
fn unavailable(status: u16) -> ProviderError {
    ProviderError::ApiError {
        provider: "controlled".into(),
        status,
        message: crate::provider_retry::with_retry_after(
            "unavailable".into(),
            Some(std::time::Duration::ZERO),
        ),
    }
}

fn answer() -> Vec<std::result::Result<StreamEvent, ProviderError>> {
    vec![
        Ok(StreamEvent::TextDelta {
            delta: "answer".into(),
        }),
        Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ]
}

fn scripted_fixture(
    script: Vec<Scripted>,
    limits: GrantLimits,
) -> (Fixture, SessionProvider, Arc<ScriptedProvider>) {
    let inner = ScriptedProvider::new(script);
    let fixture = fixture_with_limits(limits, "in-process-test");
    {
        let mut state = fixture.controller.lock().unwrap();
        let bound = state
            .bind_with_provider_gate(
                fixture.activation.clone(),
                fixture.profile.clone(),
                &serde_json::to_string(&fixture.config).unwrap(),
                AgentRunControl::new(axocoatl_actor::AgentRunId::new("activation")),
                true,
                now_ms().unwrap(),
            )
            .unwrap();
        state
            .bound
            .insert(fixture.activation.activation_id.clone(), bound);
        let snapshot = state.current(&fixture.activation).unwrap();
        let DispatchState {
            memory,
            content,
            canonical,
            ..
        } = &mut *state;
        memory
            .reserve_candidate(canonical, &fixture.activation)
            .unwrap();
        content
            .reserve_activation_output(
                &snapshot,
                &fixture.activation,
                axocoatl_session::execution_content::ActivationOutputLimits {
                    partial_records: 0,
                    partial_bytes: 0,
                    settlement_bytes: 1024 * 1024,
                },
            )
            .unwrap();
    }
    let observer = fixture
        .controller
        .stream_observer_for_test(fixture.activation.clone());
    let wrapped = SessionProvider::new(
        fixture.controller.clone(),
        fixture.activation.clone(),
        inner.clone(),
        fixture.profile.provider.clone(),
        fixture.profile.model.clone(),
    )
    .with_retry_observer(observer);
    (fixture, wrapped, inner)
}

fn roomy() -> GrantLimits {
    GrantLimits {
        activations: 8,
        invocations: 32,
        tokens: 1000,
        cost_microunits: 100,
    }
}

/// The retries recorded on the activation's stream.
fn recorded_retries(fixture: &Fixture) -> Vec<String> {
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.current(&fixture.activation).unwrap();
    state
        .content
        .activation_stream(&snapshot, &fixture.activation)
        .unwrap()
        .into_iter()
        .filter_map(|view| match view.content.payload {
            axocoatl_session::execution_content::ActivationStreamPayload::ProviderRetry {
                reason,
            } => Some(reason),
            _ => None,
        })
        .collect()
}

async fn drain(
    stream: &mut Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
) -> std::result::Result<(), ProviderError> {
    while let Some(event) = stream.next().await {
        if let StreamEvent::Done { .. } = event? {
            return Ok(());
        }
    }
    Err(ProviderError::Stream("ended without Done".into()))
}

/// A 503 before anything streamed is sent again once, as a new call with
/// its own reservation: the failed call keeps its whole reservation (1.2),
/// the retry settles to what it used, and the retry is recorded on the
/// activation before it is sent.
#[tokio::test]
async fn a_transient_failure_is_retried_once_as_a_new_reserved_call() {
    let (fixture, wrapped, inner) = scripted_fixture(
        vec![Scripted::Open(unavailable(503)), Scripted::Events(answer())],
        roomy(),
    );
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    let usage = provider_usage(&fixture);
    assert_eq!(usage.calls, 2);
    assert_eq!(usage.unsettled_calls, 0);
    // The failed call's usage is unknown, so the total is a lower bound.
    assert!(!usage.tokens.complete);
    let charged = fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .usage("grant")
        .unwrap();
    assert_eq!(charged.invocations, 2);
    assert_eq!(charged.tokens, 100 + 12, "first reservation kept, retry settled");
    let retries = recorded_retries(&fixture);
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert!(
        retries[0].starts_with("HTTP 503 from controlled; retrying once in 0 s"),
        "{retries:?}"
    );
}

/// A failure before the first streamed item (a reset, a timeout) is retried
/// too; one after the provider produced anything never is.
#[tokio::test]
async fn only_a_failure_before_anything_streamed_is_retried() {
    let (fixture, wrapped, inner) = scripted_fixture(
        vec![
            Scripted::Events(vec![Err(ProviderError::Network(
                "connection reset: the peer closed".into(),
            ))]),
            Scripted::Events(answer()),
        ],
        roomy(),
    );
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    assert_eq!(recorded_retries(&fixture).len(), 1);

    let (fixture, wrapped, inner) = scripted_fixture(
        vec![Scripted::Events(vec![
            Ok(StreamEvent::TextDelta {
                delta: "half".into(),
            }),
            Err(unavailable(503)),
        ])],
        roomy(),
    );
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::TextDelta { .. }))
    ));
    assert!(matches!(
        stream.next().await,
        Some(Err(ProviderError::ApiError { status: 503, .. }))
    ));
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    assert!(recorded_retries(&fixture).is_empty());
}

/// A second failure ends the call; rejections and refusals are never
/// retried.
#[tokio::test]
async fn a_second_failure_and_every_rejection_end_the_call() {
    let (fixture, wrapped, inner) = scripted_fixture(
        vec![
            Scripted::Open(unavailable(502)),
            Scripted::Open(unavailable(504)),
        ],
        roomy(),
    );
    let error = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ProviderError::ApiError { status: 504, .. }));
    assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider_usage(&fixture).calls, 2);
    assert_eq!(recorded_retries(&fixture).len(), 1);
    for rejected in [
        unavailable(400),
        unavailable(401),
        unavailable(402),
        unavailable(403),
        ProviderError::ContentFiltered {
            provider: "controlled".into(),
            reason: "safety".into(),
        },
    ] {
        let (fixture, wrapped, inner) =
            scripted_fixture(vec![Scripted::Open(rejected)], roomy());
        assert!(wrapped
            .chat_stream(ChatRequest::simple("input"))
            .await
            .is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_usage(&fixture).calls, 1);
        assert!(recorded_retries(&fixture).is_empty());
    }
}

/// The retry is admitted like any call: when the grant cannot pay for it
/// after the failed call kept its reservation, the original failure stands
/// and nothing more is sent.
#[tokio::test]
async fn a_retry_the_grant_cannot_pay_is_not_sent() {
    let (fixture, wrapped, inner) = scripted_fixture(
        vec![Scripted::Open(unavailable(503)), Scripted::Events(answer())],
        GrantLimits {
            tokens: 150,
            ..roomy()
        },
    );
    let error = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ProviderError::ApiError { status: 503, .. }));
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider_usage(&fixture).calls, 1);
}

/// The non-streaming path follows the same policy.
#[tokio::test]
async fn non_streaming_calls_are_retried_under_the_same_policy() {
    let (fixture, wrapped, inner) = scripted_fixture(
        vec![Scripted::Open(unavailable(429)), Scripted::Events(vec![])],
        roomy(),
    );
    let outcome = wrapped
        .chat_with_accounting(ChatRequest::simple("summarize"))
        .await;
    assert_eq!(outcome.response.unwrap().content, "answer");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider_usage(&fixture).calls, 2);
    assert_eq!(recorded_retries(&fixture).len(), 1);
}

/// A completion the provider ended with its content filter is reported as a
/// refusal, so the activation's recorded failure says the provider refused.
#[tokio::test]
async fn a_content_filter_stop_is_reported_as_a_refusal() {
    let (_fixture, wrapped, _) = scripted_fixture(
        vec![Scripted::Events(vec![
            Ok(StreamEvent::Usage(TokenUsageStats::new(10, 0))),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::ContentFilter,
            }),
        ])],
        roomy(),
    );
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("input"))
        .await
        .unwrap();
    let error = drain(&mut stream).await.err().unwrap();
    assert!(
        matches!(error, ProviderError::ContentFiltered { .. }),
        "{error:?}"
    );
    let text = format!(
        "Activation failed: {}",
        axocoatl_actor::AgentError::Provider(error.to_string())
    );
    assert_eq!(
        axocoatl_session::failure_class::classify_failure_text(&text),
        axocoatl_session::run_outcome::FailureClass::ProviderRefusal,
        "{text}"
    );
}

// ---------------------------------------- native OpenRouter over the wire

const OPENROUTER_MODEL: &str = "meta-llama/test-instruct";

async fn openrouter_metadata(server: &wiremock::MockServer) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[{
            "id": OPENROUTER_MODEL,
            "architecture": {"input_modalities":["text"],"output_modalities":["text"]},
            "supported_parameters": ["max_tokens","tools"]
        }]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{OPENROUTER_MODEL}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{
            "id": OPENROUTER_MODEL,
            "endpoints": [{
                "model_id": OPENROUTER_MODEL, "provider_name": "FiniteProvider",
                "tag": "finite/exact", "context_length": 2048, "max_completion_tokens": 128,
                "supported_parameters": ["max_tokens","tools"],
                "pricing": {"prompt":"0.000001","completion":"0.000002"},
                "status": 0, "supports_implicit_caching": false
            }]
        }})))
        .mount(server)
        .await;
}

fn openrouter_reply() -> String {
    let first = serde_json::json!({"id":"gen-fixture","model":OPENROUTER_MODEL,"provider":"FiniteProvider","choices":[{"index":0,"delta":{"content":"verified"},"finish_reason":"stop"}]});
    let usage = serde_json::json!({"id":"gen-fixture","model":OPENROUTER_MODEL,"provider":"FiniteProvider","choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,"cost":0.000008,"is_byok":false}});
    format!("data: {first}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
}

/// The Session boundary over a real native OpenRouter executor whose
/// inference endpoint answers `first` once and then the normal reply.
async fn openrouter_session(
    first: wiremock::ResponseTemplate,
) -> (wiremock::MockServer, Fixture, SessionProvider) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    openrouter_metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(first)
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(openrouter_reply(), "text/event-stream"),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    let profile = axocoatl_llm_openai::observe_native_openrouter_profiles(
        &server.uri(),
        "fixture-inference-key",
        OPENROUTER_MODEL,
    )
    .await
    .unwrap()
    .remove(0);
    let executor = Arc::new(
        axocoatl_llm_openai::NativeOpenRouterProvider::connect_observed(
            profile,
            "fixture-inference-key",
            32,
            65536,
            None,
        )
        .await
        .unwrap(),
    );
    let fixture = fixture_with_config(
        GrantLimits {
            activations: 8,
            invocations: 32,
            tokens: 100_000,
            cost_microunits: 100_000,
        },
        "in-process-test",
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "Writer".into(),
            provider: "openrouter".into(),
            model: OPENROUTER_MODEL.into(),
            tools: vec![],
            ..Default::default()
        },
        "answer once",
    );
    {
        let mut state = fixture.controller.lock().unwrap();
        let bound = state
            .bind_with_provider_gate(
                fixture.activation.clone(),
                fixture.profile.clone(),
                &serde_json::to_string(&fixture.config).unwrap(),
                AgentRunControl::new(axocoatl_actor::AgentRunId::new("activation")),
                true,
                now_ms().unwrap(),
            )
            .unwrap();
        state
            .bound
            .insert(fixture.activation.activation_id.clone(), bound);
        let snapshot = state.current(&fixture.activation).unwrap();
        let DispatchState {
            memory,
            content,
            canonical,
            ..
        } = &mut *state;
        memory
            .reserve_candidate(canonical, &fixture.activation)
            .unwrap();
        content
            .reserve_activation_output(
                &snapshot,
                &fixture.activation,
                axocoatl_session::execution_content::ActivationOutputLimits {
                    partial_records: 0,
                    partial_bytes: 0,
                    settlement_bytes: 1024 * 1024,
                },
            )
            .unwrap();
    }
    let observer = fixture
        .controller
        .stream_observer_for_test(fixture.activation.clone());
    let wrapped = SessionProvider::new(
        fixture.controller.clone(),
        fixture.activation.clone(),
        executor,
        "openrouter".into(),
        OPENROUTER_MODEL.into(),
    )
    .with_retry_observer(observer);
    (server, fixture, wrapped)
}

async fn inference_requests(server: &wiremock::MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .count()
}

/// OpenRouter answers 503 (with `Retry-After: 1`) and then the reply: the
/// Session sends the same pinned endpoint one more call after a second,
/// both calls are accounted, and the failed one keeps its reservation.
#[tokio::test]
async fn native_openrouter_503_is_retried_once_and_both_calls_are_accounted() {
    let (server, fixture, wrapped) = openrouter_session(
        wiremock::ResponseTemplate::new(503)
            .insert_header("Retry-After", "1")
            .set_body_string("upstream busy"),
    )
    .await;
    let request = ChatRequest::simple("Answer once");
    let reservation = wrapped.execution_bounds(&request).unwrap();
    let started = std::time::Instant::now();
    let mut stream = wrapped.chat_stream(request).await.unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(inference_requests(&server).await, 2);
    // Both requests named the same pinned endpoint.
    for request in server.received_requests().await.unwrap() {
        if request.method.as_str() == "POST" {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["provider"]["only"], serde_json::json!(["finite/exact"]));
            assert_eq!(body["model"], OPENROUTER_MODEL);
        }
    }
    let usage = provider_usage(&fixture);
    assert_eq!(usage.calls, 2);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(!usage.tokens.complete, "the failed call's usage is unknown");
    let charged = fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .usage("grant")
        .unwrap();
    assert_eq!(charged.invocations, 2);
    // The failed call keeps its whole reservation; the retry settles to the
    // 6 tokens and the cost OpenRouter reported for it.
    assert_eq!(charged.tokens, reservation.token_limit + 6);
    assert_eq!(charged.cost_microunits, reservation.cost_microunits + 8);
    let retries = recorded_retries(&fixture);
    assert_eq!(retries.len(), 1);
    assert!(
        retries[0].starts_with("HTTP 503 from openrouter; retrying once in 1 s"),
        "{retries:?}"
    );
    let plane = fixture.controller.control_plane().unwrap();
    let events = crate::provider_retry::run_events(&plane);
    assert!(
        matches!(events.as_slice(), [axocoatl_session::run_record::RunEvent::ProviderRetry {
            node_id, status: Some(503), ..
        }] if node_id == "counter"),
        "{events:?}"
    );
}

/// A 401 is a rejection: nothing more is sent.
#[tokio::test]
async fn native_openrouter_401_is_not_retried() {
    let (server, fixture, wrapped) = openrouter_session(
        wiremock::ResponseTemplate::new(401).set_body_string("no such key"),
    )
    .await;
    let error = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, ProviderError::ApiError { status: 401, .. }),
        "{error:?}"
    );
    assert_eq!(inference_requests(&server).await, 1);
    assert_eq!(provider_usage(&fixture).calls, 1);
    assert!(recorded_retries(&fixture).is_empty());
}
