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
