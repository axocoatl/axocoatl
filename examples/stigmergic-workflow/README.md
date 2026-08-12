# Stigmergic coordination primitive — `EventLattice` + `depends_on` DAG

This standalone example demonstrates the reusable `EventLattice` pheromone
primitive. Three agents are wired into a small dependency graph, and the order
they run in emerges from signals accumulating in the lattice. The example owns
its queue and completed guard explicitly; it is not evidence that the daemon's
session or Automation runtime uses this execution loop.

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

This is the threshold rule used by this example. It also matches the default
coordination metadata registered by `lattice_params` in `axocoatl-daemon`:

- An **entry** agent (empty `depends_on`) gets threshold `1.0` and is activated
  directly with the user's input. A `UserInput` event is published for
  observers but does **not** drive activation.
- A **downstream** agent with `N` dependencies gets threshold `0.5 × N`.
- Every `TaskCompleted` event deposits a signal of strength `0.5` onto every
  registered agent's accumulator (`EventLattice::publish` returns whoever just
  crossed their threshold).

So `implementer` (1 dependency) fires once `planner` finishes, and `reviewer`
(2 dependencies) fires only once **both** upstream agents finish — `0.5 + 0.5 =
1.0`. The example's completed guard stops an agent from running twice.

`decay_rate` is `0.0` here so the threshold math is deterministic (a join lands
on exactly `1.0`). The daemon's registered downstream metadata defaults to a
small `0.01` decay.

## Workflows vs Skills

This example is a fixed `depends_on` DAG. For event-driven *capability* routing
at the coordination-library level, see the
[`skills-lattice`](../skills-lattice) example.

## Where this fits

- `EventLattice`, `LatticeEvent`, pheromone signal state:
  [`crates/axocoatl-coordination`](../../crates/axocoatl-coordination)
- Agent coordination metadata: `lattice_params` in
  `crates/axocoatl-daemon/src/bootstrap.rs`
- Product Lattice sessions run session-scoped agents in dependency order;
  Skills, canonical Automation event triggers, the event timeline, and webhooks
  consume the daemon's event-lattice feed.
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
