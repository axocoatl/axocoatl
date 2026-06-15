# HTN planner — symbolic decomposition, LLM only at the frontier

A coordinator has to turn a goal into a list of subtasks. There are two ways to
do it, and this example shows the cheaper one.

```
cargo run -p htn-planner
```

No API keys, no network — the one place an LLM would normally be called uses a
mock resolver with a fixed reply.

## What it does

It builds a real [`HtnPlanner`](../../crates/axocoatl-coordination/src/htn.rs)
method library, plans the goal `write_feature` **symbolically**, prints the
decomposition tree, then resolves the single compound leaf the library leaves
open and re-plans into a fully primitive plan.

```
write_feature  (compound)
├─ design_feature        (compound)
│  ├─ gather_requirements [primitive]
│  └─ write_design_doc    [primitive]
├─ implement_feature     (compound)
│  ├─ write_code          [primitive]
│  └─ self_review         [primitive]
├─ test_feature          (compound)   ← method guarded by a precondition
│  ├─ write_unit_tests    [primitive]
│  └─ run_test_suite      [primitive]
└─ ship_feature          (compound)   ← no method → LLM FRONTIER
```

`write_feature` expands through methods into `design` / `implement` / `test`,
each of which expands further — all with **zero LLM calls**. `ship_feature` has
no method on purpose: it is the frontier, and the only task the model ever sees.

## HTN vs pure-LLM decomposition

The runtime can decompose a goal either way. The difference is not subtle:

| | Symbolic HTN | Pure-LLM |
|---|---|---|
| **Latency** | A tree walk over an in-memory method library — microseconds. The `htn_plan_*` benchmarks run in the nanosecond–microsecond range. | One model round-trip *per* decomposition step, each hundreds of ms to seconds. |
| **Cost** | Zero tokens for any task a method covers. The LLM is called only for uncovered frontiers — here, 1 of 7 compound tasks. | Tokens for the full goal every time, including the parts you already know how to break down. |
| **Determinism** | Identical plan every run. The same goal + method library always produces the same tree, so it is testable and replayable. | Non-deterministic shape and wording; the model can also invent tool names no declared worker has, so routing misses. |
| **Flexibility** | Limited to what the method library encodes; novel tasks fall through to the frontier. | Handles anything, including tasks nobody anticipated. |

The point is not "HTN instead of LLM" — it's **HTN first, LLM at the edges**.
Encode the parts of the workflow you understand as methods (fast, free,
deterministic) and spend model calls only on the leaves you genuinely can't
pre-decompose. That is exactly what `ship_feature` demonstrates: 6 primitives
land symbolically, and one small call resolves the rest.

## How it works

`HtnPlanner::plan(root)` walks the goal top-down:

- A **primitive** task goes straight into `plan.primitives` — it's executable.
- A **compound** task is looked up in the method library by name
  (`task_pattern == task.name`). If a method matches *and* all its
  `preconditions` hold against the planner's world state, the planner recurses
  into the method's subtasks. If no method applies, the task lands in
  `plan.llm_frontiers`.

`test_feature` shows the precondition path: its method only applies once
`design_complete == true` is in the planner's state (set via `set_state`).
Preconditions are how HTN says "you can't test what isn't designed" declaratively
instead of in control flow.

When a frontier remains, `HtnPlanner::resolve_frontiers(root, resolver,
max_rounds)` calls a [`FrontierResolver`](../../crates/axocoatl-coordination/src/htn.rs)
for each open task, registers the returned subtasks as new methods, and re-plans
— repeating until no frontiers remain or the round budget runs out. The real
resolver, [`LlmFrontierResolver`](../../crates/axocoatl-actor/src/frontier_resolver.rs),
prompts a model to return a JSON array of primitive subtasks for *one* task. This
example swaps in a deterministic mock that implements the same trait, so it runs
offline.

Finally, `OrchestrationPlan::from_plan(plan, workers)` round-robins the
primitives across workers. Each primitive carries its required tools (read from
`parameters["tools"]` via `HtnTask::required_tools()`), which is the hand-off
point to the capability auction that routes each task to a worker declaring those
tools.

## Where this lives in the real runtime

- `HtnPlanner`, `DecompositionMethod`, `FrontierResolver`, `OrchestrationPlan`:
  [`crates/axocoatl-coordination/src/htn.rs`](../../crates/axocoatl-coordination/src/htn.rs)
- The LLM-backed frontier resolver this example mocks:
  [`crates/axocoatl-actor/src/frontier_resolver.rs`](../../crates/axocoatl-actor/src/frontier_resolver.rs)
- Wiring HTN into the coordinator (`CoordinatorBehavior::with_htn_methods`):
  [`crates/axocoatl-actor/src/coordinator.rs`](../../crates/axocoatl-actor/src/coordinator.rs)
- A real methods file loaded by a workflow (`from_methods_yaml`):
  [`research-docs/htn-ship-feature.yaml`](../../research-docs/htn-ship-feature.yaml)
- HTN planning benchmarks — `htn_plan_primitive`, `htn_plan_simple_decomposition`,
  `htn_plan_nested_3_levels`, `htn_plan_with_preconditions`:
  [`benches/routing_latency.rs`](../../benches/routing_latency.rs) (the
  `// HtnPlanner benchmarks` section, lines 103–188). Run them with
  `cargo bench --bench routing_latency`.
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
