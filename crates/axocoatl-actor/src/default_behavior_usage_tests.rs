// Included in default_behavior::tests; exercise the real execution/accounting
// boundary and the native checkpoint rather than an isolated event matcher.
#[derive(Clone, Copy)]
enum ObservedUsageEnding {
    Done,
    Error,
    Eof,
    Pending,
}

struct ObservedUsageProvider {
    observations: Vec<axocoatl_core::MeasuredTokenUsage>,
    ending: ObservedUsageEnding,
    observed: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl LlmProvider for ObservedUsageProvider {
    fn provider_id(&self) -> &str {
        "usage-observation"
    }
    fn model_id(&self) -> &str {
        "usage-observation"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
        unreachable!("the actor uses streaming")
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let observations = self.observations.clone();
        let ending = self.ending;
        let observed = self.observed.clone();
        Ok(Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::TextDelta { delta: "observed answer".into() });
            for observation in observations {
                yield Ok(StreamEvent::UsageObservation(observation));
            }
            // Reaching this point requires the consumer to have processed the
            // final observation and requested another event.
            observed.notify_one();
            match ending {
                ObservedUsageEnding::Done => {
                    yield Ok(StreamEvent::Done { finish_reason: FinishReason::Stop });
                }
                ObservedUsageEnding::Error => {
                    yield Err(ProviderError::Network("terminal outcome unavailable".into()));
                }
                ObservedUsageEnding::Eof => {}
                ObservedUsageEnding::Pending => std::future::pending::<()>().await,
            }
        }))
    }
}

async fn observed_usage_run(
    observations: Vec<axocoatl_core::MeasuredTokenUsage>,
    ending: ObservedUsageEnding,
) -> (Result<crate::AgentRunOutcome, AgentError>, AgentCheckpoint) {
    let observed = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(ObservedUsageProvider {
        observations,
        ending,
        observed: observed.clone(),
    });
    let port = Arc::new(CheckpointPortProbe::new(None));
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_activation_checkpoint_port(port.clone());
    behavior
        .on_start(&activation_checkpoint_config())
        .await
        .unwrap();
    let control = activation_checkpoint_control();
    let result = if matches!(ending, ObservedUsageEnding::Pending) {
        let stop = async {
            observed.notified().await;
            control.cancel();
        };
        let run = behavior.execute_controlled(AgentInput::text("work"), control.clone());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (result, ()) = tokio::join!(run, stop);
            result
        })
        .await
        .unwrap()
    } else {
        behavior
            .execute_controlled(AgentInput::text("work"), control)
            .await
    };
    let checkpoint = {
        let staged = port.staged.lock().unwrap();
        assert_eq!(staged.len(), 1);
        serde_json::from_value::<AgentCheckpoint>(staged[0].clone()).unwrap()
    };
    (result, checkpoint)
}

#[tokio::test]
async fn explicit_usage_lower_bound_preserves_each_dimension_through_success_and_interruption() {
    for ending in [
        ObservedUsageEnding::Done,
        ObservedUsageEnding::Error,
        ObservedUsageEnding::Eof,
        ObservedUsageEnding::Pending,
    ] {
        let usage = TokenUsageStats::new(23, 7).with_reasoning(5);
        // Complete=True before interruption must still become a lower bound.
        let complete = !matches!(ending, ObservedUsageEnding::Done);
        let (result, checkpoint) = observed_usage_run(
            vec![axocoatl_core::MeasuredTokenUsage {
                usage: usage.clone(),
                complete,
            }],
            ending,
        )
        .await;
        match ending {
            ObservedUsageEnding::Done => {
                let outcome = result.unwrap();
                assert!(!outcome.is_cancelled());
                assert_eq!(outcome.output().token_usage, usage);
            }
            ObservedUsageEnding::Pending => {
                let outcome = result.unwrap();
                assert!(outcome.is_cancelled());
                assert_eq!(outcome.output().token_usage, usage);
            }
            ObservedUsageEnding::Error | ObservedUsageEnding::Eof => assert!(result.is_err()),
        }
        // A stream that ends without its completion event is retried once;
        // each attempt keeps its own reported lower bound.
        let expected = if matches!(ending, ObservedUsageEnding::Eof) {
            TokenUsageStats::new(46, 14).with_reasoning(10)
        } else {
            usage
        };
        assert_eq!(checkpoint.cumulative_token_usage, expected);
        assert!(!checkpoint.cumulative_token_usage_known);
    }
}

#[tokio::test]
async fn explicit_zero_usage_is_never_replaced_by_the_legacy_numeric_estimate() {
    for complete in [false, true] {
        let (result, checkpoint) = observed_usage_run(
            vec![axocoatl_core::MeasuredTokenUsage {
                usage: TokenUsageStats::default(),
                complete,
            }],
            ObservedUsageEnding::Done,
        )
        .await;
        let outcome = result.unwrap();
        assert_eq!(outcome.output().content, "observed answer");
        assert_eq!(outcome.output().token_usage.total(), 0);
        assert_eq!(checkpoint.cumulative_token_usage.total(), 0);
        assert_eq!(checkpoint.cumulative_token_usage_known, complete);
    }
}

#[tokio::test]
async fn cumulative_observations_replace_subtotals_and_can_be_completed_by_terminal_evidence() {
    let final_usage = TokenUsageStats::new(30, 8).with_reasoning(7);
    let (result, checkpoint) = observed_usage_run(
        vec![
            axocoatl_core::MeasuredTokenUsage::lower_bound(
                TokenUsageStats::new(23, 7).with_reasoning(5),
            ),
            axocoatl_core::MeasuredTokenUsage::known(final_usage.clone()),
        ],
        ObservedUsageEnding::Done,
    )
    .await;
    assert_eq!(result.unwrap().output().token_usage, final_usage);
    assert_eq!(checkpoint.cumulative_token_usage, final_usage);
    assert!(checkpoint.cumulative_token_usage_known);
}

#[tokio::test]
async fn regressed_explicit_usage_cannot_erase_an_already_observed_subtotal() {
    let retained = TokenUsageStats::new(23, 7).with_reasoning(5);
    let (result, checkpoint) = observed_usage_run(
        vec![
            axocoatl_core::MeasuredTokenUsage::lower_bound(retained.clone()),
            axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::new(30, 2).with_reasoning(7)),
        ],
        ObservedUsageEnding::Done,
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("usage observation decreased"));
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(30, 7).with_reasoning(7)
    );
    assert!(!checkpoint.cumulative_token_usage_known);
}
