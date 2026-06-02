# Axocoatl Architecture

A practical overview of how Axocoatl runs and coordinates agents.

## The big picture

```
            ┌─────────────────────────── axocoatl daemon ───────────────────────────┐
 CLI / HTTP │  ProviderRegistry   AgentRegistry   EventLattice   McpToolRegistry     │
   clients ─┼─▶ (per-agent LLMs)  (ractor actors)  (pheromones)   (MCP tools)         │
   (IPC)    │        │                 │                │                            │
            │        └──────── DefaultAgentBehavior ─────┘                            │
            │            session mem → budget → LLM → tools → checkpoint              │
            └────────────────────────────────────────────────────────────────────────┘
```

The **daemon** (`axocoatl-daemon`) bootstraps everything: providers, agents
(spawned as `ractor` actors), the event lattice, MCP connections, and the
activation loop. `axocoatl dev` adds a Unix-socket IPC server; `axocoatl serve`
exposes the HTTP API.

## Agents

Each agent is a `ractor` actor running `DefaultAgentBehavior`. On every turn:

1. Append input to **session memory** (Tier 1).
2. Build the request, injecting **long-term memory** (Tier 3) facts.
3. **Token budget** pre-flight check (`abort` / `warn` / `summarize`).
4. Call the agent's **provider** (Ollama, OpenAI, Anthropic, …).
5. Run any **tool calls** (built-in or MCP) with hooks, up to 10 iterations.
6. **Checkpoint** the session to disk (Tier 2) for crash recovery.

On shutdown, agents distill the session into long-term memory facts.

## Token budgets

Per-agent `token_budget` with `per_call`, `per_execution`, and an
`overflow_policy`:

- `abort` — refuse the call and terminate the agent (no wasted tokens)
- `warn` — log and continue
- `summarize` — (compaction hook)

Budgets are checked **before** the LLM call, so an over-budget request never
costs tokens.

## Stigmergic coordination

The differentiator. Agents declare `depends_on`; the daemon registers each in
an `EventLattice` with a pheromone threshold:

- **Entry agents** (`depends_on: []`) — activated directly by
  `execute_workflow` with the user input.
- **Downstream agents** — threshold = `N × 0.5` where N = number of
  dependencies. Each upstream `TaskCompleted` event emits a signal of strength
  `0.5`; when accumulated signal crosses the threshold, the agent activates and
  receives its upstream outputs as context.

There is **no scheduler**. Coordination emerges from events:

```
execute_workflow → activate entry agent
   → agent completes → publish TaskCompleted
       → lattice raises downstream pheromone signals
           → threshold crossed → downstream agent activates
               → … → all expected agents done → workflow returns
```

A cycle guard (`max_activations = agents × 3`) and acyclic-DAG validation make
runaway activation impossible.

Supporting primitives (sub-microsecond, in `axocoatl-coordination`):

- **HTN planner** — symbolic task decomposition without LLM calls.
- **Auction** — deterministic agent selection by tool capability, load, and
  remaining token budget.

## Memory tiers

| Tier | What | Persistence |
|---|---|---|
| 1 — Session | conversation transcript | in-memory |
| 2 — Checkpoint | agent state snapshots | disk (pruned to 3) |
| 3 — Long-term | distilled facts | disk (bincode) |
| 4 — Semantic | vector recall | optional feature |

## Protocols

- **MCP** — the daemon connects to configured `mcp_servers` (stdio or
  streamable-http) at bootstrap and exposes their tools to agents; agents can
  also be exposed *as* MCP tools.
- **A2A** — agent-to-agent interop for cross-framework workflows.

## Crate map

`axocoatl-core` (types) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime) ·
`axocoatl-memory` · `axocoatl-coordination` (lattice/HTN/auction) ·
`axocoatl-graph` · `axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` ·
`axocoatl-isolation` (WASM) · `axocoatl-daemon` · `axocoatl-server` ·
`axocoatl-cli`.
