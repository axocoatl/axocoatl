//! Benchmark: Actor system throughput.
//!
//! Measures spawning 100 AgentActor instances and sending messages through them
//! using ractor. Tests both spawn latency and message throughput.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ractor::Actor;
use tokio::runtime::Runtime;

use axocoatl_actor::{AgentActor, AgentBehavior, AgentError, AgentMessage};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};

/// Minimal echo behavior for benchmarking — no LLM calls, no I/O.
struct BenchEchoBehavior;

#[async_trait::async_trait]
impl AgentBehavior for BenchEchoBehavior {
    async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), AgentError> {
        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        Ok(AgentOutput {
            content: input.content,
            tool_calls: vec![],
            token_usage: TokenUsageStats::new(1, 1),
        })
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        Ok(())
    }
}

fn test_config(id: &str) -> AgentConfig {
    AgentConfig {
        id: AgentId::new(id),
        name: format!("Bench Agent {id}"),
        ..AgentConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Spawn benchmarks
// ---------------------------------------------------------------------------

/// Benchmark spawning a single agent actor.
fn bench_spawn_single_actor(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("actor_spawn_single", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (actor_ref, handle) = AgentActor::spawn(
                    Some("bench-single".to_string()),
                    AgentActor,
                    (test_config("bench-single"), Box::new(BenchEchoBehavior)),
                )
                .await
                .unwrap();
                actor_ref.stop(None);
                handle.await.unwrap();
            });
        });
    });
}

/// Benchmark spawning 100 agent actors concurrently.
fn bench_spawn_100_actors(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("actor_spawn_100", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut actors = Vec::with_capacity(100);
                for i in 0..100 {
                    let name = format!("bench-{i}");
                    let (actor_ref, handle) = AgentActor::spawn(
                        Some(name.clone()),
                        AgentActor,
                        (test_config(&name), Box::new(BenchEchoBehavior)),
                    )
                    .await
                    .unwrap();
                    actors.push((actor_ref, handle));
                }

                // Stop all actors
                for (actor_ref, handle) in actors {
                    actor_ref.stop(None);
                    handle.await.unwrap();
                }
            });
        });
    });
}

// ---------------------------------------------------------------------------
// Message throughput benchmarks
// ---------------------------------------------------------------------------

/// Benchmark sending a single Execute message to one actor.
fn bench_single_message(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let (actor_ref, _handle) = rt.block_on(async {
        AgentActor::spawn(
            Some("bench-msg".to_string()),
            AgentActor,
            (test_config("bench-msg"), Box::new(BenchEchoBehavior)),
        )
        .await
        .unwrap()
    });

    c.bench_function("actor_single_message", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                actor_ref
                    .cast(AgentMessage::Execute {
                        input: AgentInput::text("bench"),
                        reply: tx,
                        sink: None,
                    })
                    .unwrap();
                let result = rx.await.unwrap();
                let _ = black_box(result);
            });
        });
    });

    rt.block_on(async {
        actor_ref.stop(None);
    });
}

/// Benchmark sending 100 messages sequentially to one actor.
fn bench_sequential_messages(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let (actor_ref, _handle) = rt.block_on(async {
        AgentActor::spawn(
            Some("bench-seq".to_string()),
            AgentActor,
            (test_config("bench-seq"), Box::new(BenchEchoBehavior)),
        )
        .await
        .unwrap()
    });

    c.bench_function("actor_100_sequential_messages", |b| {
        b.iter(|| {
            rt.block_on(async {
                for _ in 0..100 {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    actor_ref
                        .cast(AgentMessage::Execute {
                            input: AgentInput::text("bench"),
                            reply: tx,
                            sink: None,
                        })
                        .unwrap();
                    let result = rx.await.unwrap();
                    let _ = black_box(result);
                }
            });
        });
    });

    rt.block_on(async {
        actor_ref.stop(None);
    });
}

/// Benchmark spawning 100 actors and sending one message to each.
fn bench_fanout_100_actors(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("actor_fanout_100", |b| {
        b.iter(|| {
            rt.block_on(async {
                // Spawn 100 actors
                let mut actors = Vec::with_capacity(100);
                for i in 0..100 {
                    let name = format!("fanout-{i}");
                    let (actor_ref, handle) = AgentActor::spawn(
                        Some(name.clone()),
                        AgentActor,
                        (test_config(&name), Box::new(BenchEchoBehavior)),
                    )
                    .await
                    .unwrap();
                    actors.push((actor_ref, handle));
                }

                // Send one message to each and collect replies
                let mut receivers = Vec::with_capacity(100);
                for (actor_ref, _) in &actors {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    actor_ref
                        .cast(AgentMessage::Execute {
                            input: AgentInput::text("fanout"),
                            reply: tx,
                            sink: None,
                        })
                        .unwrap();
                    receivers.push(rx);
                }

                // Await all replies
                for rx in receivers {
                    let result = rx.await.unwrap();
                    let _ = black_box(result);
                }

                // Stop all actors
                for (actor_ref, handle) in actors {
                    actor_ref.stop(None);
                    handle.await.unwrap();
                }
            });
        });
    });
}

/// Benchmark GetStatus message (lightweight, no execution).
fn bench_status_query(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let (actor_ref, _handle) = rt.block_on(async {
        AgentActor::spawn(
            Some("bench-status".to_string()),
            AgentActor,
            (test_config("bench-status"), Box::new(BenchEchoBehavior)),
        )
        .await
        .unwrap()
    });

    c.bench_function("actor_status_query", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                actor_ref.cast(AgentMessage::GetStatus(tx)).unwrap();
                let status = rx.await.unwrap();
                black_box(status);
            });
        });
    });

    rt.block_on(async {
        actor_ref.stop(None);
    });
}

criterion_group!(
    benches,
    bench_spawn_single_actor,
    bench_spawn_100_actors,
    bench_single_message,
    bench_sequential_messages,
    bench_fanout_100_actors,
    bench_status_query,
);
criterion_main!(benches);
