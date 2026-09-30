//! Revocation at the actual post-provider safe boundary preserves retained evidence.
use super::*;

struct RevocationFactory {
    inner: NestedFactory,
    started: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for RevocationFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let mut resources = self.inner.resources(input).await?;
        if resources.config.role == AgentRole::Worker {
            resources.provider = Arc::new(HeldWorker {
                started: self.started.clone(),
                release: self.release.clone(),
            });
        }
        Ok(resources)
    }
}
struct HeldWorker {
    started: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}
#[async_trait::async_trait]
impl LlmProvider for HeldWorker {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
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
            cost_microunits: 0,
            response_bytes: 8192,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        self.started.fetch_add(1, Ordering::SeqCst);
        let permit = self.release.acquire().await.unwrap();
        permit.forget();
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                delta: "Already claimed provider returned".into(),
            }),
            Ok(StreamEvent::Usage(TokenUsageStats::new(5, 5))),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[tokio::test]
async fn revoked_parent_at_child_provider_return_cancels_without_poisoning_history() {
    let fixture = coordinator_fixture(100000).await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let factory = Arc::new(RevocationFactory {
        inner: NestedFactory {
            controller: controller.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            child_calls: Arc::new(AtomicUsize::new(0)),
            resolved: std::sync::Mutex::new(vec![]),
            gate: None,
        },
        started: started.clone(),
        release: release.clone(),
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        factory.clone(),
    )
    .unwrap() else {
        panic!("native driver")
    };
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while started.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let before = controller.snapshot().unwrap();
        let children = before
            .contract()
            .activations()
            .iter()
            .filter(|item| item.activation.node_id != fixture.request.node_evidence[0].node_id)
            .map(|item| item.activation.clone())
            .collect::<Vec<_>>();
        assert_eq!(children.len(), 2);
        controller
            .revoke_control_grant("coordinator-grant", 1)
            .unwrap();
        release.add_permits(2);
        children
    };
    let (result, children) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(prepared.run(), revoke)
    })
    .await
    .unwrap();
    let result = result.unwrap();
    assert_eq!(
        result.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(result
        .snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    for child in children {
        let item = result
            .snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == child)
            .unwrap();
        assert_eq!(item.state, ActivationState::Failed);
        let usage = controller.activation_provider_usage(&child).unwrap();
        assert_eq!(usage.calls, 1);
        assert!(usage.tokens.complete);
        assert_eq!(usage.tokens.usage, TokenUsageStats::new(5, 5));
    }
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert!(
        controller.history_snapshot().is_ok(),
        "ordinary revocation leaves history readable"
    );
    assert!(controller.control_plane().is_ok());
    assert!(fixture.registry.live_native_turns().unwrap().is_empty());
}
