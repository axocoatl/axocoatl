//! HTN planner — symbolic task decomposition, LLM only at the frontier.
//!
//! A coordinator can break a goal into subtasks two ways:
//!
//! - **Pure-LLM:** hand the whole goal to a model and let it invent a subtask
//!   list. Flexible, but every plan costs a network round-trip, the shape is
//!   non-deterministic, and the model can name tools no worker actually has.
//! - **Symbolic HTN:** expand the goal through a hand-written *method library*.
//!   A method says "task X decomposes into subtasks A, B, C." Expansion is a
//!   tree walk — no tokens, no network, identical output every run. The model
//!   is called **only** for the leaves no method covers (the *frontiers*).
//!
//! This example builds a real [`HtnPlanner`] method library, plans a feature
//! goal **symbolically**, prints the decomposition tree, and then resolves the
//! one compound leaf the library deliberately leaves open with a *mock* frontier
//! resolver — so the whole thing runs with no API keys and no network.
//!
//! ```text
//!   write_feature  (compound)
//!   ├─ design_feature        (compound)
//!   │  ├─ gather_requirements [primitive]
//!   │  └─ write_design_doc    [primitive]
//!   ├─ implement_feature     (compound)
//!   │  ├─ write_code          [primitive]
//!   │  └─ self_review         [primitive]
//!   ├─ test_feature          (compound)
//!   │  ├─ write_unit_tests    [primitive]
//!   │  └─ run_test_suite      [primitive]
//!   └─ ship_feature          (compound)  ← no method → LLM FRONTIER
//! ```
//!
//! `write_feature` → `design`/`implement`/`test` expand from methods with zero
//! LLM calls. `ship_feature` has no method on purpose: it is the frontier the
//! model resolves. After resolution, re-planning folds its new subtasks in and
//! the plan is fully primitive.
//!
//! Run: `cargo run -p htn-planner` (no API keys — mock frontier resolver).

use std::collections::HashMap;

use async_trait::async_trait;

use axocoatl_coordination::{
    Condition, DecompositionMethod, FrontierResolver, HtnPlan, HtnPlanner, HtnTask, HtnTaskType,
    OrchestrationPlan,
};

// ---------------------------------------------------------------------------
// Task constructors — `HtnTask` is { name, parameters, task_type }. The planner
// matches a method's `task_pattern` against `task.name`, so the names here are
// the join keys with the method library below.
// ---------------------------------------------------------------------------

fn primitive(name: &str, tools: &[&str]) -> HtnTask {
    let mut parameters = HashMap::new();
    if !tools.is_empty() {
        // `HtnTask::required_tools()` reads parameters["tools"] as a JSON string
        // array — the same convention the auction uses to match workers.
        parameters.insert("tools".to_string(), serde_json::json!(tools));
    }
    HtnTask {
        name: name.to_string(),
        parameters,
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

// ---------------------------------------------------------------------------
// Method library — the entire "intelligence" of symbolic planning lives here.
// Each method maps one compound task name to its ordered subtasks. There is no
// method for `ship_feature`, so it stays a frontier until the LLM resolves it.
// ---------------------------------------------------------------------------

fn build_planner() -> HtnPlanner {
    let mut planner = HtnPlanner::new();

    // write_feature → design + implement + test + ship
    planner.add_method(DecompositionMethod {
        task_pattern: "write_feature".to_string(),
        preconditions: vec![],
        subtasks: vec![
            compound("design_feature"),
            compound("implement_feature"),
            compound("test_feature"),
            compound("ship_feature"),
        ],
    });

    // design_feature → two primitives
    planner.add_method(DecompositionMethod {
        task_pattern: "design_feature".to_string(),
        preconditions: vec![],
        subtasks: vec![
            primitive("gather_requirements", &["doc_search"]),
            primitive("write_design_doc", &["doc_write"]),
        ],
    });

    // implement_feature → two primitives
    planner.add_method(DecompositionMethod {
        task_pattern: "implement_feature".to_string(),
        preconditions: vec![],
        subtasks: vec![
            primitive("write_code", &["code_edit"]),
            primitive("self_review", &["code_read"]),
        ],
    });

    // test_feature → guarded by a precondition. The method only applies once
    // `design_complete == true` is in world state; before that, test_feature
    // would fall through to the frontier. This is HTN's "you can't test what
    // isn't designed" expressed declaratively, not in control flow.
    planner.add_method(DecompositionMethod {
        task_pattern: "test_feature".to_string(),
        preconditions: vec![Condition {
            key: "design_complete".to_string(),
            expected: serde_json::json!(true),
        }],
        subtasks: vec![
            primitive("write_unit_tests", &["code_edit"]),
            primitive("run_test_suite", &["shell"]),
        ],
    });

    // NOTE: no method for `ship_feature` — left as the LLM frontier on purpose.

    // Satisfy the test_feature precondition so its method applies.
    planner.set_state("design_complete", serde_json::json!(true));

    planner
}

// ---------------------------------------------------------------------------
// Mock frontier resolver — stands in for the LLM. The real one
// (`axocoatl_actor::LlmFrontierResolver`) prompts a model to return a JSON array
// of primitive subtasks; here we return a fixed decomposition so the example is
// deterministic and offline. It implements the SAME `FrontierResolver` trait the
// planner calls, so swapping in a live provider is a one-line change.
// ---------------------------------------------------------------------------

struct MockShipResolver;

#[async_trait]
impl FrontierResolver for MockShipResolver {
    async fn resolve(
        &self,
        task: &HtnTask,
        _state: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<HtnTask>, String> {
        // A live model would receive only THIS task ("ship_feature"), not the
        // whole goal — that is the cost win: one small call for the one leaf the
        // method library doesn't cover.
        if task.name != "ship_feature" {
            return Err(format!(
                "mock resolver only knows ship_feature, got {}",
                task.name
            ));
        }
        // Emitted as Primitive so re-planning converges (the real resolver does
        // the same — it never emits further compound tasks).
        Ok(vec![
            primitive("open_pull_request", &["git", "github"]),
            primitive("deploy_to_staging", &["shell", "ci"]),
        ])
    }
}

// ---------------------------------------------------------------------------
// Tree rendering — `HtnPlan` is a FLAT result (primitives + llm_frontiers); it
// does not retain the hierarchy. To show the decomposition *tree*, we walk the
// methods ourselves via the planner's public `decompose()`, exactly the way
// `plan()` recurses internally. This mutates nothing — it is a read-only view of
// what `plan()` will produce.
// ---------------------------------------------------------------------------

fn print_tree(planner: &HtnPlanner, task: &HtnTask, prefix: &str, is_last: bool, is_root: bool) {
    let branch = if is_root {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };

    let tools = task.required_tools();
    let tool_note = if tools.is_empty() {
        String::new()
    } else {
        format!("  tools={tools:?}")
    };

    match task.task_type {
        HtnTaskType::Primitive => {
            println!("{prefix}{branch}{} [primitive]{tool_note}", task.name);
        }
        HtnTaskType::Compound => match planner.decompose(task) {
            Some(subtasks) => {
                println!("{prefix}{branch}{} (compound)", task.name);
                let child_prefix = if is_root {
                    String::new()
                } else if is_last {
                    format!("{prefix}   ")
                } else {
                    format!("{prefix}│  ")
                };
                let n = subtasks.len();
                for (i, sub) in subtasks.iter().enumerate() {
                    print_tree(planner, sub, &child_prefix, i + 1 == n, false);
                }
            }
            None => {
                // No method applies (missing method OR unmet precondition) → the
                // model gets called here, and only here.
                println!("{prefix}{branch}{} (compound)  ← LLM FRONTIER", task.name);
            }
        },
    }
}

fn print_flat_plan(plan: &HtnPlan) {
    println!(
        "  Primitives ({}) — ready to schedule, zero LLM calls:",
        plan.primitives.len()
    );
    for (i, t) in plan.primitives.iter().enumerate() {
        let tools = t.required_tools();
        if tools.is_empty() {
            println!("    {}. {}", i + 1, t.name);
        } else {
            println!("    {}. {}  tools={tools:?}", i + 1, t.name);
        }
    }
    println!(
        "  Frontiers ({}) — need the LLM to decompose:",
        plan.llm_frontiers.len()
    );
    for (i, t) in plan.llm_frontiers.iter().enumerate() {
        println!("    {}. {}", i + 1, t.name);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: HTN Planner (symbolic decomposition, LLM only at the frontier) ===\n");

    let mut planner = build_planner();
    let goal = compound("write_feature");

    // -----------------------------------------------------------------------
    // 1. SYMBOLIC PLAN — expand the goal through the method library. This is a
    //    pure tree walk: no network, no tokens, deterministic. We print the tree
    //    BEFORE any agent or model runs.
    // -----------------------------------------------------------------------
    println!("Goal: {} (compound)\n", goal.name);
    println!("── Symbolic decomposition tree (no LLM, no network) ──\n");
    print_tree(&planner, &goal, "", true, true);

    // `plan()` returns the flat schedulable result the coordinator consumes.
    let symbolic_plan = planner.plan(goal.clone());
    println!("\n── plan() result — flattened from the tree above ──\n");
    print_flat_plan(&symbolic_plan);

    println!(
        "\n{} primitives resolved symbolically; {} frontier left for the LLM.",
        symbolic_plan.primitives.len(),
        symbolic_plan.llm_frontiers.len()
    );

    // -----------------------------------------------------------------------
    // 2. RESOLVE THE FRONTIER — only `ship_feature` reaches the model. The mock
    //    resolver implements the real `FrontierResolver` trait; `resolve_frontiers`
    //    calls it for each frontier, registers the returned subtasks as new
    //    methods, and re-plans until nothing is left open (bounded by max_rounds).
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(78));
    println!(
        "\nResolving {} frontier task(s) with the (mock) LLM resolver...",
        symbolic_plan.llm_frontiers.len()
    );
    for f in &symbolic_plan.llm_frontiers {
        println!("  → calling resolver for: {}", f.name);
    }

    let resolver = MockShipResolver;
    let resolved_plan = planner
        .resolve_frontiers(goal.clone(), &resolver, 4)
        .await
        .map_err(|e| format!("frontier resolution failed: {e}"))?;

    println!("\n── Fully-resolved plan (after frontier resolution + re-plan) ──\n");
    print_flat_plan(&resolved_plan);

    if resolved_plan.llm_frontiers.is_empty() {
        println!("\nAll frontiers resolved — the plan is fully primitive and schedulable.");
    } else {
        println!(
            "\n{} frontier(s) still unresolved after the round budget.",
            resolved_plan.llm_frontiers.len()
        );
    }

    // -----------------------------------------------------------------------
    // 3. ASSIGN TO WORKERS — `OrchestrationPlan::from_plan` round-robins the
    //    primitives across available workers. This is the hand-off point to the
    //    coordinator/auction: every primitive carries its required tools, so a
    //    capability auction can route each to the worker that declares them.
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(78));
    let workers = vec![
        "worker-frontend".to_string(),
        "worker-backend".to_string(),
        "worker-qa".to_string(),
    ];
    let orchestration = OrchestrationPlan::from_plan(resolved_plan, &workers);

    println!(
        "\n── Worker assignments (round-robin over {} workers) ──\n",
        workers.len()
    );
    for (worker, task) in &orchestration.assignments {
        let tools = task.required_tools();
        println!("  {worker:<16} ← {:<22} tools={tools:?}", task.name);
    }
    if !orchestration.unassigned.is_empty() {
        println!("\n  Unassigned ({}):", orchestration.unassigned.len());
        for t in &orchestration.unassigned {
            println!("    • {}", t.name);
        }
    }

    println!(
        "\n{} primitives assigned across {} workers, {} unassigned.",
        orchestration.assignments.len(),
        workers.len(),
        orchestration.unassigned.len()
    );

    println!("\n=== Done ===");
    Ok(())
}
