//! Benchmark: Coordination layer routing latency.
//!
//! Measures EventLattice::publish, HtnPlanner::plan, and auction engine (compute_bid + run_auction).
//! Target: <1ms for each operation.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::collections::HashMap;

use axocoatl_coordination::{
    compute_bid, run_auction, AgentBid, Condition, DecompositionMethod, EventId, EventLattice,
    EventType, HtnPlanner, HtnTask, HtnTaskType, LatticeEvent,
};
use axocoatl_core::{AgentConfig, AgentId};

fn now_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn task_event(task_type: &str) -> LatticeEvent {
    LatticeEvent {
        id: EventId::random(),
        event_type: EventType::TaskAvailable {
            task_type: task_type.to_string(),
        },
        payload: serde_json::json!({}),
        produced_by: "bench".to_string(),
        timestamp: now_timestamp(),
    }
}

fn primitive(name: &str) -> HtnTask {
    HtnTask {
        name: name.to_string(),
        parameters: HashMap::new(),
        task_type: HtnTaskType::Primitive,
    }
}

fn compound(name: &str) -> HtnTask {
    HtnTask {
        name: name.to_string(),
        parameters: HashMap::new(),
        task_type: HtnTaskType::Compound,
    }
}

fn agent_with_tools(id: &str, tools: Vec<&str>) -> AgentConfig {
    AgentConfig {
        id: AgentId::new(id),
        name: id.to_string(),
        tools: tools.into_iter().map(String::from).collect(),
        ..AgentConfig::default()
    }
}

// ---------------------------------------------------------------------------
// EventLattice benchmarks
// ---------------------------------------------------------------------------

/// Publish a single event to a lattice with no registered agents.
fn bench_lattice_publish_no_agents(c: &mut Criterion) {
    let lattice = EventLattice::new(1024);
    c.bench_function("lattice_publish_no_agents", |b| {
        b.iter(|| {
            let activated = lattice.publish(task_event("research"));
            black_box(activated);
        });
    });
}

/// Publish a single event to a lattice with 10 registered agents.
fn bench_lattice_publish_10_agents(c: &mut Criterion) {
    let lattice = EventLattice::new(1024);
    for i in 0..10 {
        lattice.register_agent(AgentId::new(format!("agent-{i}")), 5.0, 0.0);
    }
    c.bench_function("lattice_publish_10_agents", |b| {
        b.iter(|| {
            let activated = lattice.publish(task_event("research"));
            black_box(activated);
        });
    });
}

/// Publish a single event to a lattice with 100 registered agents.
fn bench_lattice_publish_100_agents(c: &mut Criterion) {
    let lattice = EventLattice::new(4096);
    for i in 0..100 {
        lattice.register_agent(AgentId::new(format!("agent-{i}")), 50.0, 0.0);
    }
    c.bench_function("lattice_publish_100_agents", |b| {
        b.iter(|| {
            let activated = lattice.publish(task_event("research"));
            black_box(activated);
        });
    });
}

// ---------------------------------------------------------------------------
// HtnPlanner benchmarks
// ---------------------------------------------------------------------------

/// Plan a single primitive task (no decomposition needed).
fn bench_htn_plan_primitive(c: &mut Criterion) {
    let planner = HtnPlanner::new();
    c.bench_function("htn_plan_primitive", |b| {
        b.iter(|| {
            let plan = planner.plan(primitive("do_thing"));
            black_box(plan.primitives.len());
        });
    });
}

/// Plan a compound task with a single decomposition step (2 primitives).
fn bench_htn_plan_simple_decomposition(c: &mut Criterion) {
    let mut planner = HtnPlanner::new();
    planner.add_method(DecompositionMethod {
        task_pattern: "research".to_string(),
        preconditions: vec![],
        subtasks: vec![primitive("search"), primitive("summarize")],
    });

    c.bench_function("htn_plan_simple_decomposition", |b| {
        b.iter(|| {
            let plan = planner.plan(compound("research"));
            black_box(plan.primitives.len());
        });
    });
}

/// Plan a deeply nested compound task (3 levels, 5 total primitives).
fn bench_htn_plan_nested_decomposition(c: &mut Criterion) {
    let mut planner = HtnPlanner::new();
    planner.add_method(DecompositionMethod {
        task_pattern: "build_report".to_string(),
        preconditions: vec![],
        subtasks: vec![compound("gather_data"), primitive("format_output")],
    });
    planner.add_method(DecompositionMethod {
        task_pattern: "gather_data".to_string(),
        preconditions: vec![],
        subtasks: vec![compound("fetch"), primitive("validate")],
    });
    planner.add_method(DecompositionMethod {
        task_pattern: "fetch".to_string(),
        preconditions: vec![],
        subtasks: vec![primitive("query_db"), primitive("call_api")],
    });

    c.bench_function("htn_plan_nested_3_levels", |b| {
        b.iter(|| {
            let plan = planner.plan(compound("build_report"));
            black_box(plan.primitives.len());
        });
    });
}

/// Plan with precondition checks.
fn bench_htn_plan_with_preconditions(c: &mut Criterion) {
    let mut planner = HtnPlanner::new();
    planner.set_state("tests_passing", serde_json::json!(true));
    planner.set_state("env", serde_json::json!("production"));

    planner.add_method(DecompositionMethod {
        task_pattern: "deploy".to_string(),
        preconditions: vec![
            Condition {
                key: "tests_passing".to_string(),
                expected: serde_json::json!(true),
            },
            Condition {
                key: "env".to_string(),
                expected: serde_json::json!("production"),
            },
        ],
        subtasks: vec![primitive("push"), primitive("notify"), primitive("monitor")],
    });

    c.bench_function("htn_plan_with_preconditions", |b| {
        b.iter(|| {
            let plan = planner.plan(compound("deploy"));
            black_box(plan.primitives.len());
        });
    });
}

// ---------------------------------------------------------------------------
// Auction engine benchmarks
// ---------------------------------------------------------------------------

/// Compute a single bid.
fn bench_auction_compute_bid(c: &mut Criterion) {
    let agent = agent_with_tools("researcher", vec!["web_search", "read_file", "summarize"]);
    let required = vec!["web_search".to_string(), "read_file".to_string()];

    c.bench_function("auction_compute_bid", |b| {
        b.iter(|| {
            let bid = compute_bid(black_box(&agent), black_box(&required), 2, 5000);
            black_box(bid);
        });
    });
}

/// Run an auction with 10 bidders.
fn bench_auction_10_bidders(c: &mut Criterion) {
    let bids: Vec<AgentBid> = (0..10)
        .map(|i| AgentBid {
            agent_id: AgentId::new(format!("agent-{i}")),
            score: (i as f32) * 0.1,
        })
        .collect();

    c.bench_function("auction_run_10_bidders", |b| {
        b.iter(|| {
            let winner = run_auction(black_box(bids.clone()));
            black_box(winner);
        });
    });
}

/// Full auction pipeline: compute 10 bids + select winner.
fn bench_auction_full_pipeline(c: &mut Criterion) {
    let agents: Vec<AgentConfig> = (0..10)
        .map(|i| {
            agent_with_tools(
                &format!("agent-{i}"),
                vec!["web_search", "read_file", "summarize"],
            )
        })
        .collect();
    let required = vec!["web_search".to_string()];

    c.bench_function("auction_full_pipeline_10", |b| {
        b.iter(|| {
            let bids: Vec<AgentBid> = agents
                .iter()
                .enumerate()
                .map(|(i, agent)| compute_bid(agent, &required, i % 5, 5000 - i * 100))
                .collect();
            let winner = run_auction(bids);
            black_box(winner);
        });
    });
}

criterion_group!(
    benches,
    bench_lattice_publish_no_agents,
    bench_lattice_publish_10_agents,
    bench_lattice_publish_100_agents,
    bench_htn_plan_primitive,
    bench_htn_plan_simple_decomposition,
    bench_htn_plan_nested_decomposition,
    bench_htn_plan_with_preconditions,
    bench_auction_compute_bid,
    bench_auction_10_bidders,
    bench_auction_full_pipeline,
);
criterion_main!(benches);
