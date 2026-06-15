# Stigmergic workflow — `EventLattice` + `depends_on` DAG

Axocoatl's headline claim is **no central orchestrator**. This example proves
it: nothing in the code says "run B after A." Three agents are wired into a
small dependency graph, and the order they run in *emerges* from signals
accumulating on the shared coordination space — the `EventLattice`.

```
cargo run
```

No API keys — it uses a mock LLM with one canned reply per role.

## The graph

```
        planner ──completes──▶ implementer ──completes──▶ reviewer
           └──────────────────────────────────────────────▶┘
        (reviewer depends on BOTH planner and implementer)
```

| agent       | `depends_on`           | threshold | fires when                          |
|-------------|------------------------|-----------|-------------------------------------|
| planner     | (none)                 | 1.0       | directly, at kickoff                |
| implementer | planner                | 0.5       | after 1 upstream completes (`0.5`)  |
| reviewer    | planner, implementer   | 1.0       | after 2 upstream complete (`1.0`)   |

## The pheromone math

This is exactly the rule the daemon uses (`lattice_params` in
`axocoatl-daemon`):

- An **entry** agent (empty `depends_on`) gets threshold `1.0` and is activated
  directly with the user's input. A `UserInput` event is published for
  observers but does **not** drive activation.
- A **downstream** agent with `N` dependencies gets threshold `0.5 × N`.
- Every `TaskCompleted` event deposits a signal of strength `0.5` onto every
  registered agent's accumulator (`EventLattice::publish` returns whoever just
  crossed their threshold).

So `implementer` (1 dependency) fires once `planner` finishes, and `reviewer`
(2 dependencies) fires only once **both** upstream agents finish — `0.5 + 0.5 =
1.0`. A completed-guard stops an agent from running twice, the same way the
daemon's activation loop skips an agent that already has a result.

`decay_rate` is `0.0` here so the threshold math is deterministic (a join lands
on exactly `1.0`). The daemon defaults downstream agents to a small `0.01`
decay so stale signals expire on long-running graphs.

## Workflows vs Skills

This is a **workflow**: a `depends_on` DAG with a defined shape. For
event-driven *capability* routing — where publishing one event fans out to
every agent that declares it `reacts_to` that event, with no fixed graph — see
the [`skills-lattice`](../skills-lattice) example.

## Where this lives in the real runtime

- `EventLattice`, `LatticeEvent`, pheromone signal state:
  [`crates/axocoatl-coordination`](../../crates/axocoatl-coordination)
- The production activation loop this mirrors:
  `crates/axocoatl-daemon/src/activation.rs`
- The threshold rule: `lattice_params` in `crates/axocoatl-daemon/src/bootstrap.rs`
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
