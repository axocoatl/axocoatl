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
/// after the failed call kept its reservation, the original failure stands,
/// nothing more is sent, and no retry is recorded.
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
    assert!(recorded_retries(&fixture).is_empty());
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
    let (fixture, wrapped) = openrouter_session_on(&server).await;
    (server, fixture, wrapped)
}

/// The Session boundary over a real native OpenRouter executor holding
/// every endpoint `server`'s catalog lists for [`OPENROUTER_MODEL`],
/// cheapest first, as Team & budget retains them for failover.
async fn openrouter_session_on(server: &wiremock::MockServer) -> (Fixture, SessionProvider) {
    let (fixture, wrapped, _, _) = openrouter_session_served(server, |_| 100_000).await;
    (fixture, wrapped)
}

/// [`openrouter_session_on`] with a grant of `cost(executor)` microdollars,
/// also returning the executor and where it records the endpoints that
/// answered.
async fn openrouter_session_served(
    server: &wiremock::MockServer,
    cost: impl FnOnce(&axocoatl_llm_openai::NativeOpenRouterProvider) -> u64,
) -> (
    Fixture,
    SessionProvider,
    Arc<axocoatl_llm_openai::NativeOpenRouterProvider>,
    Arc<axocoatl_llm_openai::NativeOpenRouterServed>,
) {
    let profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
        &server.uri(),
        "fixture-inference-key",
        OPENROUTER_MODEL,
    )
    .await
    .unwrap();
    let served = Arc::new(axocoatl_llm_openai::NativeOpenRouterServed::default());
    let executor = Arc::new(
        axocoatl_llm_openai::NativeOpenRouterProvider::connect_observed_endpoints(
            profiles,
            "fixture-inference-key",
            32,
            65536,
            None,
        )
        .await
        .unwrap()
        .with_served_endpoints(served.clone()),
    );
    let cost = cost(&executor);
    let (fixture, wrapped) = session_over_with(executor.clone(), OPENROUTER_MODEL, cost);
    (fixture, wrapped, executor, served)
}

/// Each endpoint calls went to: (tag, calls answered, calls moved away).
fn served_counts(
    served: &axocoatl_llm_openai::NativeOpenRouterServed,
) -> Vec<(String, u32, u32)> {
    served
        .endpoints()
        .into_iter()
        .map(|served| (served.endpoint, served.calls, served.moved))
        .collect()
}

/// The Session boundary of one activation of an OpenRouter Agent on `model`
/// over `executor`, with a grant of 100,000 tokens and $0.10.
fn session_over(executor: Arc<dyn LlmProvider>, model: &str) -> (Fixture, SessionProvider) {
    session_over_with(executor, model, 100_000)
}

/// [`session_over`] with a grant of `cost_microunits`.
fn session_over_with(
    executor: Arc<dyn LlmProvider>,
    model: &str,
    cost_microunits: u64,
) -> (Fixture, SessionProvider) {
    let fixture = fixture_with_config(
        GrantLimits {
            activations: 8,
            invocations: 32,
            tokens: 100_000,
            cost_microunits,
        },
        "in-process-test",
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "Writer".into(),
            provider: "openrouter".into(),
            model: model.into(),
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
        model.into(),
    )
    .with_retry_observer(observer);
    (fixture, wrapped)
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
/// Session sends the same pinned endpoint (the model's only one) one more
/// call after a second, and both calls are accounted. OpenRouter refused
/// the first with a status before generating anything, so it settles at no
/// tokens and no cost, and the call's usage stays exact.
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
    assert_eq!(usage.unreported_calls, 0);
    assert!(usage.tokens.complete, "the refused call is known to be free");
    assert!(usage.cost_known);
    assert_eq!(usage.cost_microunits, 8);
    let charged = fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .usage("grant")
        .unwrap();
    assert_eq!(charged.invocations, 2);
    // The refused call gives its reservation back; the retry settles to the
    // 6 tokens and the cost OpenRouter reported for it.
    assert!(reservation.token_limit > 6);
    assert_eq!(charged.tokens, 6);
    assert_eq!(charged.cost_microunits, 8);
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

// ------------------------------------- native OpenRouter endpoint failover

/// What OpenRouter answered on 2026-10-23 when a model's cheapest endpoint
/// was limited upstream: its raw text carries a hint to add a provider key
/// (BYOK), which native Sessions refuse.
const UPSTREAM_LIMITED: &str = r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"meta-llama/test-instruct is temporarily rate-limited upstream. Please retry shortly, or add your own key to accumulate your rate limits: https://openrouter.ai/settings/integrations","provider_name":"FiniteProvider","is_byok":false}},"user_id":"user_fixture"}"#;

/// One endpoint of [`OPENROUTER_MODEL`]: `tag`, served by `provider`, at
/// `scale` times the fixture's prices.
fn openrouter_endpoint(tag: &str, provider: &str, scale: u32) -> serde_json::Value {
    serde_json::json!({
        "model_id": OPENROUTER_MODEL, "provider_name": provider, "tag": tag,
        "context_length": 2048, "max_completion_tokens": 128,
        "supported_parameters": ["max_tokens","tools"],
        "pricing": {"prompt": format!("0.00000{scale}"), "completion": format!("0.00000{}", 2 * scale)},
        "status": 0, "supports_implicit_caching": false
    })
}

/// A catalog listing `endpoints` for [`OPENROUTER_MODEL`] (`up_to` reads
/// of them, then `later` for every later read, when given).
async fn openrouter_catalog(
    server: &wiremock::MockServer,
    endpoints: Vec<serde_json::Value>,
    later: Option<(u64, Vec<serde_json::Value>)>,
) {
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
    let rows = |endpoints: Vec<serde_json::Value>| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{
            "id": OPENROUTER_MODEL, "endpoints": endpoints
        }}))
    };
    let endpoints_path = format!("/models/{OPENROUTER_MODEL}/endpoints");
    match later {
        Some((up_to, later)) => {
            Mock::given(method("GET"))
                .and(path(endpoints_path.as_str()))
                .respond_with(rows(endpoints))
                .up_to_n_times(up_to)
                .with_priority(1)
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path(endpoints_path.as_str()))
                .respond_with(rows(later))
                .with_priority(2)
                .mount(server)
                .await;
        }
        None => {
            Mock::given(method("GET"))
                .and(path(endpoints_path.as_str()))
                .respond_with(rows(endpoints))
                .mount(server)
                .await;
        }
    }
}

/// Every inference request pinned to `tag` answers `template`.
async fn openrouter_answers(
    server: &wiremock::MockServer,
    tag: &str,
    template: wiremock::ResponseTemplate,
) {
    use wiremock::matchers::{body_partial_json, method, path};
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"provider":{"only":[tag]}})))
        .respond_with(template)
        .mount(server)
        .await;
}

/// The reply `provider` streams for [`OPENROUTER_MODEL`].
fn openrouter_reply_from(provider: &str) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(200).set_body_raw(
        openrouter_reply().replace("FiniteProvider", provider),
        "text/event-stream",
    )
}

/// The endpoint each inference request `server` received pinned, in
/// order. Each pins exactly one endpoint, with no OpenRouter fallback.
async fn pinned_endpoints(server: &wiremock::MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["provider"]["only"], body["provider"]["order"]);
            assert_eq!(body["provider"]["allow_fallbacks"], false);
            assert_eq!(body["model"], OPENROUTER_MODEL);
            body["provider"]["only"][0].as_str().unwrap().to_owned()
        })
        .collect()
}

fn charged(fixture: &Fixture) -> axocoatl_session::control_authority::GrantUsage {
    fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .usage("grant")
        .unwrap()
}

/// The cheapest endpoint is rate-limited upstream (HTTP 429) while another
/// endpoint of the model is healthy: the call is sent again at once to the
/// next endpoint, which answers. The refused call costs nothing, the run's
/// record says which endpoint the call moved from and to, and later calls
/// stay on the endpoint that answered.
#[tokio::test]
async fn native_openrouter_429_on_one_endpoint_moves_the_call_to_the_next_at_once() {
    let server = wiremock::MockServer::start().await;
    openrouter_catalog(
        &server,
        vec![
            openrouter_endpoint("finite/exact", "FiniteProvider", 1),
            openrouter_endpoint("other/exact", "OtherProvider", 2),
        ],
        None,
    )
    .await;
    openrouter_answers(
        &server,
        "finite/exact",
        wiremock::ResponseTemplate::new(429).set_body_string(UPSTREAM_LIMITED),
    )
    .await;
    openrouter_answers(&server, "other/exact", openrouter_reply_from("OtherProvider")).await;
    let (fixture, wrapped) = openrouter_session_on(&server).await;
    let started = std::time::Instant::now();
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "no backoff before moving to a healthy endpoint"
    );
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact"]
    );
    let usage = provider_usage(&fixture);
    assert_eq!(usage.calls, 2);
    assert_eq!(usage.unreported_calls, 0);
    assert!(usage.tokens.complete && usage.cost_known);
    assert_eq!(usage.cost_microunits, 8);
    let charged = charged(&fixture);
    assert_eq!((charged.invocations, charged.tokens, charged.cost_microunits), (2, 6, 8));
    let notes = recorded_retries(&fixture);
    assert_eq!(
        notes,
        [format!(
            "HTTP 429 from openrouter; the call was sent again at once, from endpoint \
             finite/exact (FiniteProvider) to endpoint other/exact (OtherProvider) of the same \
             model ({OPENROUTER_MODEL}), as a new, separately reserved call"
        )]
    );
    let plane = fixture.controller.control_plane().unwrap();
    assert!(matches!(
        crate::provider_retry::run_events(&plane).as_slice(),
        [axocoatl_session::run_record::RunEvent::ProviderRetry { status: Some(429), .. }]
    ));
    // The next call goes to the endpoint that answered.
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Answer again"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact", "other/exact"]
    );
    assert_eq!(recorded_retries(&fixture).len(), 1);
}

/// Every endpoint is rate-limited: after each one refused the call, the
/// Session waits once, as long as the longest `Retry-After` they stated (at
/// most 30 s; here the first endpoint's, not the latest's), tries each
/// again, and then ends the call saying plainly that OpenRouter reports
/// every endpoint as rate-limited, a provider outage, with what to do;
/// never OpenRouter's raw JSON or its hint to add a provider key. Every
/// refusal costs nothing, so the usage stays exact.
#[tokio::test]
async fn native_openrouter_every_endpoint_limited_waits_once_then_says_it_is_an_outage() {
    let server = wiremock::MockServer::start().await;
    openrouter_catalog(
        &server,
        vec![
            openrouter_endpoint("finite/exact", "FiniteProvider", 1),
            openrouter_endpoint("other/exact", "OtherProvider", 2),
        ],
        None,
    )
    .await;
    openrouter_answers(
        &server,
        "finite/exact",
        wiremock::ResponseTemplate::new(429)
            .insert_header("Retry-After", "2")
            .set_body_string(UPSTREAM_LIMITED),
    )
    .await;
    openrouter_answers(
        &server,
        "other/exact",
        wiremock::ResponseTemplate::new(429)
            .insert_header("Retry-After", "1")
            .set_body_string(UPSTREAM_LIMITED.replace("FiniteProvider", "OtherProvider")),
    )
    .await;
    let (fixture, wrapped, _, served) = openrouter_session_served(&server, |_| 100_000).await;
    let started = std::time::Instant::now();
    let error = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .err()
        .unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact", "other/exact", "finite/exact"]
    );
    let ProviderError::ApiError {
        status: 429,
        message,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(
        message,
        &format!(
            "OpenRouter reports every endpoint of {OPENROUTER_MODEL} that this Agent may use as \
             rate-limited or unavailable right now (finite/exact (FiniteProvider): rate-limited \
             (HTTP 429); other/exact (OtherProvider): rate-limited (HTTP 429)). This is a \
             provider outage, not your setup: try again later, or choose another model; nothing \
             from them reached the Agent (Retry-After: 2 s)"
        )
    );
    for leaked in ["{", "own key", "integrations", "user_"] {
        assert!(!message.contains(leaked), "{leaked}: {message}");
    }
    // No endpoint answered; each failed the call that then moved on once.
    assert_eq!(
        served_counts(&served),
        [
            ("finite/exact".to_owned(), 0, 1),
            ("other/exact".to_owned(), 0, 1)
        ]
    );
    let usage = provider_usage(&fixture);
    assert_eq!(usage.calls, 4);
    assert_eq!(usage.unreported_calls, 0);
    assert!(usage.tokens.complete && usage.cost_known);
    let charged = charged(&fixture);
    assert_eq!((charged.invocations, charged.tokens, charged.cost_microunits), (4, 0, 0));
    let notes = recorded_retries(&fixture);
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert!(notes[0].contains("from endpoint finite/exact (FiniteProvider) to endpoint other/exact"));
    assert!(notes[1].starts_with("HTTP 429 from openrouter; retrying once in 2 s"), "{notes:?}");
    assert!(notes[2].contains("from endpoint other/exact (OtherProvider) to endpoint finite/exact"));
}

/// OpenRouter's limit on the account is every endpoint's: the call is not
/// moved, but waits its `Retry-After` and is sent again to the same one.
#[tokio::test]
async fn native_openrouter_account_limit_is_waited_out_on_the_same_endpoint() {
    let server = wiremock::MockServer::start().await;
    openrouter_catalog(
        &server,
        vec![
            openrouter_endpoint("finite/exact", "FiniteProvider", 1),
            openrouter_endpoint("other/exact", "OtherProvider", 2),
        ],
        None,
    )
    .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_string(r#"{"error":{"message":"Rate limit exceeded: free-models-per-min.","code":429,"metadata":{"provider_name":null}}}"#),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    openrouter_answers(&server, "finite/exact", openrouter_reply_from("FiniteProvider")).await;
    let (fixture, wrapped) = openrouter_session_on(&server).await;
    let started = std::time::Instant::now();
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "finite/exact"]
    );
    let notes = recorded_retries(&fixture);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("HTTP 429 from openrouter; retrying once in 1 s"), "{notes:?}");
    assert!(provider_usage(&fixture).tokens.complete);
}

/// Inference requests pinned to `tag` answer `first` `times` times, then
/// `then`.
async fn openrouter_answers_then(
    server: &wiremock::MockServer,
    tag: &str,
    first: wiremock::ResponseTemplate,
    times: u64,
    then: wiremock::ResponseTemplate,
) {
    use wiremock::matchers::{body_partial_json, method, path};
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"provider":{"only":[tag]}})))
        .respond_with(first)
        .up_to_n_times(times)
        .with_priority(1)
        .mount(server)
        .await;
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"provider":{"only":[tag]}})))
        .respond_with(then)
        .with_priority(2)
        .mount(server)
        .await;
}

/// Two endpoints of [`OPENROUTER_MODEL`]: `finite/exact` and, at `scale`
/// (at most 4) times its prices, `other/exact`.
async fn two_endpoints(server: &wiremock::MockServer, scale: u32) {
    openrouter_catalog(
        server,
        vec![
            openrouter_endpoint("finite/exact", "FiniteProvider", 1),
            openrouter_endpoint("other/exact", "OtherProvider", scale),
        ],
        None,
    )
    .await;
}

/// An upstream 429 of `provider` that asks to wait `seconds`.
fn limited_for(provider: &str, seconds: u64) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(429)
        .insert_header("Retry-After", seconds.to_string().as_str())
        .set_body_string(UPSTREAM_LIMITED.replace("FiniteProvider", provider))
}

/// An endpoint whose `Retry-After` is longer than the 30 s the policy
/// allows sits the next round out: it is never sent to again before that
/// time. The round waits for the others' longest `Retry-After` and resumes
/// on the latest endpoint; the call ends once that one fails again.
#[tokio::test]
async fn native_openrouter_an_endpoint_asking_to_wait_too_long_sits_the_next_round_out() {
    let server = wiremock::MockServer::start().await;
    two_endpoints(&server, 2).await;
    openrouter_answers(&server, "finite/exact", limited_for("FiniteProvider", 60)).await;
    openrouter_answers(&server, "other/exact", limited_for("OtherProvider", 1)).await;
    let (fixture, wrapped) = openrouter_session_on(&server).await;
    let started = std::time::Instant::now();
    let error = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .err()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_secs(1)
            && elapsed < std::time::Duration::from_secs(20),
        "{elapsed:?}"
    );
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact", "other/exact"],
        "the endpoint that asked for 60 s is not sent to again"
    );
    let message = error.to_string();
    assert!(
        message.contains(
            "OpenRouter reports every endpoint of meta-llama/test-instruct that this Agent may \
             use as rate-limited or unavailable right now (finite/exact (FiniteProvider): \
             rate-limited (HTTP 429); other/exact (OtherProvider): rate-limited (HTTP 429))"
        ) && message.ends_with("(Retry-After: 1 s)"),
        "{message}"
    );
    let notes = recorded_retries(&fixture);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes[1].starts_with("HTTP 429 from openrouter; retrying once in 1 s on the same"));
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (3, 0));
}

/// When the latest endpoint asks to wait longer than the policy allows but
/// an earlier one of the round does not, the call waits for that one and
/// is sent to it again, recorded as one retry that names the move.
#[tokio::test]
async fn native_openrouter_a_round_resumes_on_an_endpoint_that_may_be_sent_to_again() {
    let server = wiremock::MockServer::start().await;
    two_endpoints(&server, 2).await;
    openrouter_answers_then(
        &server,
        "finite/exact",
        limited_for("FiniteProvider", 1),
        1,
        openrouter_reply_from("FiniteProvider"),
    )
    .await;
    openrouter_answers(&server, "other/exact", limited_for("OtherProvider", 60)).await;
    let (fixture, wrapped, executor, served) =
        openrouter_session_served(&server, |_| 100_000).await;
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact", "finite/exact"]
    );
    assert_eq!(executor.endpoint_failover().unwrap().current_endpoint(), 0);
    let notes = recorded_retries(&fixture);
    assert_eq!(
        notes[1],
        format!(
            "HTTP 429 from openrouter; retrying once in 1 s, from endpoint other/exact \
             (OtherProvider) to endpoint finite/exact (FiniteProvider) of the same model \
             ({OPENROUTER_MODEL}), as a new, separately reserved call"
        )
    );
    assert_eq!(notes.len(), 2);
    assert_eq!(
        served_counts(&served),
        [
            ("finite/exact".to_owned(), 1, 1),
            ("other/exact".to_owned(), 0, 1)
        ]
    );
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (3, 0));
    assert!(usage.tokens.complete && usage.cost_known);
}

/// The failover analogue of `a_retry_the_grant_cannot_pay_is_not_sent`:
/// the next endpoint costs four times as much, more than the grant has
/// left, so the call is not moved there (and calls do not stay there).
/// Instead the retry policy sends it to the same endpoint again after its
/// wait, and the activation's next call is admitted there too.
#[tokio::test]
async fn native_openrouter_a_move_the_grant_cannot_pay_is_not_made() {
    let server = wiremock::MockServer::start().await;
    two_endpoints(&server, 4).await;
    openrouter_answers_then(
        &server,
        "finite/exact",
        limited_for("FiniteProvider", 0),
        1,
        openrouter_reply_from("FiniteProvider"),
    )
    .await;
    openrouter_answers(&server, "other/exact", openrouter_reply_from("OtherProvider")).await;
    let request = ChatRequest::simple("Answer once");
    let (fixture, wrapped, executor, _) = openrouter_session_served(&server, |executor| {
        3 * executor.execution_bounds(&request).unwrap().cost_microunits
    })
    .await;
    let failover = executor.endpoint_failover().unwrap();
    let cheap = executor.execution_bounds(&request).unwrap();
    let pricier = failover.execution_bounds_on(1, &request).unwrap();
    assert!(
        pricier.cost_microunits > 3 * cheap.cost_microunits,
        "{cheap:?} {pricier:?}"
    );
    let mut stream = wrapped.chat_stream(request.clone()).await.unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    assert_eq!(failover.current_endpoint(), 0);
    let notes = recorded_retries(&fixture);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("HTTP 429 from openrouter; retrying once in 0 s on the same"),
        "{notes:?}"
    );
    // The next call is admitted on the endpoint calls stayed on.
    let mut stream = wrapped.chat_stream(request).await.unwrap();
    drain(&mut stream).await.unwrap();
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "finite/exact", "finite/exact"]
    );
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (3, 0));
}

/// Calls moved to a pricier endpoint, and the grant can no longer pay for
/// the next call there: it is admitted on the cheaper endpoint that can
/// carry it, and calls go there, rather than being refused as if the
/// budget were used up.
#[tokio::test]
async fn native_openrouter_a_call_the_grant_cannot_pay_where_calls_moved_goes_to_a_cheaper_one() {
    let server = wiremock::MockServer::start().await;
    two_endpoints(&server, 2).await;
    openrouter_answers_then(
        &server,
        "finite/exact",
        limited_for("FiniteProvider", 0),
        1,
        openrouter_reply_from("FiniteProvider"),
    )
    .await;
    openrouter_answers(&server, "other/exact", openrouter_reply_from("OtherProvider")).await;
    let request = ChatRequest::simple("Answer once");
    // Enough for one call on the pricier endpoint, not for two.
    let (fixture, wrapped, executor, served) = openrouter_session_served(&server, |executor| {
        executor
            .endpoint_failover()
            .unwrap()
            .execution_bounds_on(1, &request)
            .unwrap()
            .cost_microunits
            + 4
    })
    .await;
    let mut stream = wrapped.chat_stream(request.clone()).await.unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    let failover = executor.endpoint_failover().unwrap();
    assert_eq!(failover.current_endpoint(), 1);
    let mut stream = wrapped.chat_stream(request).await.unwrap();
    drain(&mut stream).await.unwrap();
    assert_eq!(failover.current_endpoint(), 0);
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact", "finite/exact"]
    );
    assert_eq!(recorded_retries(&fixture).len(), 1, "a new call is no retry");
    assert_eq!(
        served_counts(&served),
        [
            ("finite/exact".to_owned(), 1, 1),
            ("other/exact".to_owned(), 1, 0)
        ]
    );
}

/// OpenRouter's stream for `provider`: its keep-alive comment, then an
/// error inside the answer with `code`.
fn error_in_answer(provider: &str, chunks: &[serde_json::Value]) -> wiremock::ResponseTemplate {
    let mut body = String::from(": OPENROUTER PROCESSING\n\n");
    for chunk in chunks {
        let mut chunk = chunk.clone();
        chunk["id"] = "gen-failed".into();
        chunk["model"] = OPENROUTER_MODEL.into();
        chunk["provider"] = provider.into();
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// Every endpoint reports a 429 inside its answer (HTTP 200, then an error
/// chunk), the form OpenRouter uses once it started the stream. The calls
/// move and wait as for a refused one, the last attempt's failure included,
/// and the call ends with the same plain words: never one endpoint's error.
/// An answer had started on each, so none is known to be free: each call
/// keeps its reservation and is named as one that did not report.
#[tokio::test]
async fn native_openrouter_errors_inside_every_answer_are_said_as_an_outage() {
    let server = wiremock::MockServer::start().await;
    two_endpoints(&server, 2).await;
    let failure = |provider: &str| {
        error_in_answer(
            provider,
            &[serde_json::json!({"error":{"code":429,"message":"Rate limit exceeded"},"choices":[]})],
        )
    };
    openrouter_answers(&server, "finite/exact", failure("FiniteProvider")).await;
    openrouter_answers(&server, "other/exact", failure("OtherProvider")).await;
    let (fixture, wrapped, _, served) = openrouter_session_served(&server, |_| 100_000).await;
    let error = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .err()
        .unwrap();
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact", "other/exact", "finite/exact"]
    );
    let ProviderError::ApiError {
        status: 429,
        message,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(
        message,
        &format!(
            "OpenRouter reports every endpoint of {OPENROUTER_MODEL} that this Agent may use as \
             rate-limited or unavailable right now (finite/exact (FiniteProvider): rate-limited \
             (HTTP 429); other/exact (OtherProvider): rate-limited (HTTP 429)). This is a \
             provider outage, not your setup: try again later, or choose another model; nothing \
             from them reached the Agent"
        )
    );
    assert_eq!(recorded_retries(&fixture).len(), 3);
    assert_eq!(
        served_counts(&served),
        [
            ("finite/exact".to_owned(), 0, 1),
            ("other/exact".to_owned(), 0, 1)
        ]
    );
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (4, 4));
    assert!(!usage.tokens.complete);
}

/// An error inside an answer is never taken as proof that the call cost
/// nothing, whatever its message says: here the endpoint streamed
/// reasoning (which OpenRouter bills) before an error whose message says
/// "nothing was generated". The call keeps its whole reservation, is named
/// as one that did not report, and moves to the next endpoint.
#[tokio::test]
async fn native_openrouter_an_error_inside_an_answer_keeps_its_reservation() {
    let server = wiremock::MockServer::start().await;
    two_endpoints(&server, 2).await;
    openrouter_answers(
        &server,
        "finite/exact",
        error_in_answer(
            "FiniteProvider",
            &[
                serde_json::json!({"choices":[{"index":0,"delta":{"reasoning":"thinking it over"}}]}),
                serde_json::json!({"error":{"code":502,"message":"Upstream aborted: nothing was \
                    generated after the cutoff; nothing was sent to the model"},"choices":[]}),
            ],
        ),
    )
    .await;
    openrouter_answers(&server, "other/exact", openrouter_reply_from("OtherProvider")).await;
    let request = ChatRequest::simple("Answer once");
    let (fixture, wrapped, executor, served) =
        openrouter_session_served(&server, |_| 100_000).await;
    let reservation = executor.execution_bounds(&request).unwrap();
    let mut stream = wrapped.chat_stream(request).await.unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact"]
    );
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (2, 1));
    assert!(!usage.tokens.complete);
    let charged = charged(&fixture);
    assert_eq!(charged.tokens, reservation.token_limit + 6);
    assert_eq!(charged.cost_microunits, reservation.cost_microunits + 8);
    assert_eq!(
        served_counts(&served),
        [
            ("finite/exact".to_owned(), 0, 1),
            ("other/exact".to_owned(), 1, 0)
        ]
    );
}

/// The endpoint a call moves to is checked against the catalog before the
/// call is sent, as the first is: one whose contract changed since Team &
/// budget reviewed it (here its price rose) is never sent to; the call
/// moves on to the next that still matches. Both refusals cost nothing.
#[tokio::test]
async fn native_openrouter_fallback_whose_contract_changed_is_skipped_unsent() {
    let server = wiremock::MockServer::start().await;
    let endpoints = |other_scale| {
        vec![
            openrouter_endpoint("finite/exact", "FiniteProvider", 1),
            openrouter_endpoint("other/exact", "OtherProvider", other_scale),
            openrouter_endpoint("third/exact", "ThirdProvider", 3),
        ]
    };
    // As Team & budget observed it, as the provider opens, and before the
    // first call; the other endpoint's price rises after.
    openrouter_catalog(&server, endpoints(2), Some((3, endpoints(4)))).await;
    openrouter_answers(
        &server,
        "finite/exact",
        wiremock::ResponseTemplate::new(429).set_body_string(UPSTREAM_LIMITED),
    )
    .await;
    openrouter_answers(&server, "other/exact", openrouter_reply_from("OtherProvider")).await;
    openrouter_answers(&server, "third/exact", openrouter_reply_from("ThirdProvider")).await;
    let (fixture, wrapped) = openrouter_session_on(&server).await;
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "third/exact"]
    );
    let notes = recorded_retries(&fixture);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[1].contains("other/exact (OtherProvider)")
            && notes[1].contains("changed since Team & budget reviewed it")
            && notes[1].ends_with(&format!(
                "from endpoint other/exact (OtherProvider) to endpoint third/exact \
                 (ThirdProvider) of the same model ({OPENROUTER_MODEL}), as a new, separately \
                 reserved call"
            )),
        "{notes:?}"
    );
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (3, 0));
    assert!(usage.tokens.complete && usage.cost_known);
}

/// An endpoint a call moved to whose answer names another provider than
/// the one it pinned is refused, as on the first endpoint: the answer never
/// reaches the Agent, and the call is not moved again.
#[tokio::test]
async fn native_openrouter_fallback_that_answers_as_another_provider_is_refused() {
    let server = wiremock::MockServer::start().await;
    openrouter_catalog(
        &server,
        vec![
            openrouter_endpoint("finite/exact", "FiniteProvider", 1),
            openrouter_endpoint("other/exact", "OtherProvider", 2),
            openrouter_endpoint("third/exact", "ThirdProvider", 3),
        ],
        None,
    )
    .await;
    openrouter_answers(
        &server,
        "finite/exact",
        wiremock::ResponseTemplate::new(429).set_body_string(UPSTREAM_LIMITED),
    )
    .await;
    openrouter_answers(&server, "other/exact", openrouter_reply_from("FiniteProvider")).await;
    openrouter_answers(&server, "third/exact", openrouter_reply_from("ThirdProvider")).await;
    let (fixture, wrapped) = openrouter_session_on(&server).await;
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Answer once"))
        .await
        .unwrap();
    let mut text = String::new();
    let error = loop {
        match stream.next().await {
            Some(Ok(StreamEvent::TextDelta { delta })) => text.push_str(&delta),
            Some(Ok(_)) => {}
            Some(Err(error)) => break error,
            None => panic!("the answer of another provider must be refused"),
        }
    };
    drop(stream);
    assert!(text.is_empty(), "{text}");
    assert!(
        error
            .to_string()
            .contains("response changed the exact admitted provider"),
        "{error}"
    );
    assert_eq!(
        pinned_endpoints(&server).await,
        ["finite/exact", "other/exact"]
    );
    // The refused answer keeps its reservation: its usage is not known.
    let usage = provider_usage(&fixture);
    assert_eq!((usage.calls, usage.unreported_calls), (2, 1));
    assert!(!usage.tokens.complete);
}

/// Live, against OpenRouter itself: a model's retained cheapest endpoint
/// that cannot take a call now (its retained price is changed here, so the
/// catalog check before the call refuses it and nothing is sent) moves the
/// call at once to the model's next retained endpoint, which answers as
/// itself. The moved call costs nothing, the answer's usage and billed cost
/// are exact, and the record names the move. Spends well under $0.001.
///
/// ```text
/// OPENROUTER_API_KEY=... cargo test -p axocoatl-daemon --lib openrouter_live_failover -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and spends OpenRouter credit"]
async fn openrouter_live_failover_moves_a_call_to_the_next_endpoint() {
    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("the live OpenRouter test needs OPENROUTER_API_KEY in its environment");
    let model = std::env::var("AXOCOATL_LIVE_FAILOVER_MODEL")
        .unwrap_or_else(|_| "qwen/qwen3-coder-30b-a3b-instruct".into());
    let mut profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
        "https://openrouter.ai/api/v1",
        &key,
        &model,
    )
    .await
    .unwrap();
    assert!(
        profiles.len() >= 2,
        "{model} needs two healthy endpoints now: {profiles:?}"
    );
    profiles.truncate(2);
    let (first, second) = (profiles[0].clone(), profiles[1].clone());
    // A retained price that no longer matches the catalog: a changed
    // contract, refused before anything is sent.
    profiles[0].prompt_price_per_million = format!("{}1", profiles[0].prompt_price_per_million);
    let served = Arc::new(axocoatl_llm_openai::NativeOpenRouterServed::default());
    let executor = Arc::new(
        axocoatl_llm_openai::NativeOpenRouterProvider::connect_observed_endpoints(
            profiles, &key, 32, 65536, None,
        )
        .await
        .unwrap()
        .with_served_endpoints(served.clone()),
    );
    let (fixture, wrapped) = session_over(executor, &model);
    let mut stream = wrapped
        .chat_stream(ChatRequest::simple("Reply with the single word: ok"))
        .await
        .unwrap();
    drain(&mut stream).await.unwrap();
    drop(stream);
    let notes = recorded_retries(&fixture);
    let usage = provider_usage(&fixture);
    let charged = charged(&fixture);
    println!(
        "{}",
        serde_json::json!({
            "model": model,
            "notes": notes,
            "served": served.endpoints().iter().map(|served| serde_json::json!({
                "endpoint": served.endpoint, "provider": served.provider,
                "calls": served.calls, "moved": served.moved,
            })).collect::<Vec<_>>(),
            "provider_calls": usage.calls,
            "unreported_calls": usage.unreported_calls,
            "usage_complete": usage.tokens.complete,
            "cost_known": usage.cost_known,
            "cost_microusd": usage.cost_microunits,
            "grant_tokens_charged": charged.tokens,
            "grant_cost_charged_microusd": charged.cost_microunits,
        })
    );
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(&format!(
            "from endpoint {} ({}) to endpoint {} ({})",
            first.endpoint_tag, first.provider_name, second.endpoint_tag, second.provider_name
        )),
        "{notes:?}"
    );
    let endpoints = served.endpoints();
    assert_eq!(
        endpoints
            .iter()
            .map(|served| (served.endpoint.as_str(), served.calls, served.moved))
            .collect::<Vec<_>>(),
        [
            (first.endpoint_tag.as_str(), 0, 1),
            (second.endpoint_tag.as_str(), 1, 0)
        ]
    );
    assert_eq!((usage.calls, usage.unreported_calls), (2, 0));
    assert!(usage.tokens.complete && usage.cost_known, "{usage:?}");
    assert!(usage.cost_microunits > 0);
    assert_eq!(charged.tokens, usage.tokens.usage.total() as u64);
    assert_eq!(charged.cost_microunits, usage.cost_microunits);
}
