//! Frame adjacent display deltas after raw provider accounting and before actor
//! observation. A frame is acknowledged/published only by the existing durable
//! observer. Tool, usage and terminal boundaries never overtake pending text.
use super::*;
use std::future::Future;
use std::time::Duration;

const FRAME_BYTES: usize = 256;
const FRAME_DELAY: Duration = Duration::from_millis(250);

pub(super) struct FramedProviderStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
    pending: Option<(bool, String)>,
    boundary: Option<Result<StreamEvent, ProviderError>>,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    ended: bool,
}

impl FramedProviderStream {
    pub(super) fn new(
        inner: Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
    ) -> Self {
        Self {
            inner,
            pending: None,
            boundary: None,
            deadline: None,
            ended: false,
        }
    }

    fn take_frame(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        self.deadline = None;
        self.pending.take().map(|(reasoning, delta)| {
            Ok(if reasoning {
                StreamEvent::ReasoningDelta { delta }
            } else {
                StreamEvent::TextDelta { delta }
            })
        })
    }
}

impl Stream for FramedProviderStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(boundary) = self.boundary.take() {
            return Poll::Ready(Some(boundary));
        }
        if self.ended {
            return Poll::Ready(self.take_frame());
        }
        // Bound work even for a provider emitting empty, immediately ready deltas.
        for _ in 0..64 {
            if self
                .deadline
                .as_mut()
                .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready())
            {
                return Poll::Ready(self.take_frame());
            }
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(
                    event @ (StreamEvent::TextDelta { .. } | StreamEvent::ReasoningDelta { .. }),
                ))) => {
                    let (reasoning, delta) = match event {
                        StreamEvent::TextDelta { delta } => (false, delta),
                        StreamEvent::ReasoningDelta { delta } => (true, delta),
                        _ => unreachable!(),
                    };
                    if delta.is_empty() {
                        continue;
                    }
                    if let Some((previous_kind, previous)) = &mut self.pending {
                        if *previous_kind == reasoning
                            && previous.len().saturating_add(delta.len()) <= FRAME_BYTES
                        {
                            previous.push_str(&delta);
                            if previous.len() == FRAME_BYTES {
                                return Poll::Ready(self.take_frame());
                            }
                        } else {
                            self.boundary = Some(Ok(if reasoning {
                                StreamEvent::ReasoningDelta { delta }
                            } else {
                                StreamEvent::TextDelta { delta }
                            }));
                            return Poll::Ready(self.take_frame());
                        }
                    } else if delta.len() >= FRAME_BYTES {
                        return Poll::Ready(Some(Ok(if reasoning {
                            StreamEvent::ReasoningDelta { delta }
                        } else {
                            StreamEvent::TextDelta { delta }
                        })));
                    } else {
                        self.pending = Some((reasoning, delta));
                        self.deadline = Some(Box::pin(tokio::time::sleep(FRAME_DELAY)));
                    }
                }
                Poll::Ready(Some(boundary)) => {
                    if let Some(frame) = self.take_frame() {
                        self.boundary = Some(boundary);
                        return Poll::Ready(Some(frame));
                    }
                    return Poll::Ready(Some(boundary));
                }
                Poll::Ready(None) => {
                    self.ended = true;
                    return Poll::Ready(self.take_frame());
                }
                Poll::Pending => {
                    if self
                        .deadline
                        .as_mut()
                        .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready())
                    {
                        return Poll::Ready(self.take_frame());
                    }
                    return Poll::Pending;
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn frames_preserve_order_and_flush_before_usage_failure_and_end() {
        let events = vec![
            Ok(StreamEvent::ReasoningDelta {
                delta: "first".into(),
            }),
            Ok(StreamEvent::ReasoningDelta {
                delta: " second".into(),
            }),
            Ok(StreamEvent::Usage(TokenUsageStats::new(3, 2))),
            Ok(StreamEvent::TextDelta {
                delta: "answer".into(),
            }),
            Err(ProviderError::Stream("failed".into())),
            Ok(StreamEvent::TextDelta {
                delta: "tail".into(),
            }),
        ];
        let mut framed = FramedProviderStream::new(Box::pin(tokio_stream::iter(events)));
        assert!(
            matches!(framed.next().await, Some(Ok(StreamEvent::ReasoningDelta { delta })) if delta == "first second")
        );
        assert!(matches!(
            framed.next().await,
            Some(Ok(StreamEvent::Usage(_)))
        ));
        assert!(
            matches!(framed.next().await, Some(Ok(StreamEvent::TextDelta { delta })) if delta == "answer")
        );
        assert!(matches!(framed.next().await, Some(Err(_))));
        assert!(
            matches!(framed.next().await, Some(Ok(StreamEvent::TextDelta { delta })) if delta == "tail")
        );
        assert!(framed.next().await.is_none());
    }

    #[tokio::test]
    async fn stalled_provider_flushes_bounded_pending_frame() {
        let input = tokio_stream::iter(vec![Ok(StreamEvent::TextDelta {
            delta: "visible".into(),
        })])
        .chain(tokio_stream::pending());
        let mut framed = FramedProviderStream::new(Box::pin(input));
        let next = tokio::time::timeout(Duration::from_secs(2), framed.next())
            .await
            .unwrap();
        assert!(matches!(next, Some(Ok(StreamEvent::TextDelta { delta })) if delta == "visible"));
    }
}
