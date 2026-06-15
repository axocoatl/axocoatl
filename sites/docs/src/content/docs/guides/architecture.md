---
title: Architecture
description: "The mental model behind the codebase: lattice, actors, supervisors, memory tiers, isolation."
---

# Axocoatl Architecture

A practical overview of how Axocoatl runs and coordinates agents.

## The big picture

```
            ┌─────────────────────────── axocoatl daemon ───────────────────────────┐
 CLI / HTTP │  ProviderRegistry   AgentRegistry   EventLattice   McpToolRegistry     │
   clients ─┼─▶ (per-agent LLMs)  (ractor actors)  (pheromones)   (MCP tools)         │
   (IPC)    │        │                 │                │                            │
            │        └──────── DefaultAgentBehavior ─────┘                            │
            │       session mem → memory → budget → LLM → tools → checkpoint          │
            └────────────────────────────────────────────────────────────────────────┘
```

The **daemon** (`axocoatl-daemon`) bootstraps everything: providers, agents
(spawned as `ractor` actors), the event lattice, MCP connections, and the
activation loop. `axocoatl dev` adds a Unix-socket IPC server; `axocoatl serve`
exposes the HTTP API.

## Agents

Each agent is a `ractor` actor running `DefaultAgentBehavior`. On every turn:

1. Append input to **session memory** (Tier 1).
2. Build the request, injecting **memory context** — the agent's editable
   **core-memory** blocks (Tier 3) plus passive top-k **semantic recall**
   (Tier 4).
3. **Token budget** pre-flight check (`abort` / `warn`).
4. Call the agent's **provider** (Ollama, OpenAI, Anthropic, …).
5. Run any **tool calls** (built-in or MCP) with hooks, up to 10 iterations.
6. **Checkpoint** the session to disk for crash recovery.

Idle agents run a background **sleep-time consolidation** pass: an LLM
memory-manager promotes durable Tier-4 facts into the agent's core-memory
blocks (promotion-only — it never evicts semantic memory). The same pass runs
once more on a graceful stop.

## Token budgets

Per-agent `token_budget` with `per_call`, `per_execution`, and an
`overflow_policy`:

- `abort` — refuse the call and terminate the agent (no wasted tokens)
- `warn` — log and continue

Budgets are checked **before** the LLM call, so an over-budget request never
costs tokens. Both the `per_call` (single-call) and `per_execution`
(cumulative) limits are enforced pre-flight. (`summarize` is a deprecated YAML
alias that now maps to `warn` — context compaction is automatic and
independent of the spend budget; see [Memory tiers](#memory-tiers).)

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

This stigmergic event lattice is the **shipped** coordination layer. There is
no scheduler and no central orchestrator — coordination is entirely emergent
from `depends_on` declarations and `TaskCompleted` signal strength.

### Hierarchical coordinator (role-based orchestration)

The lattice is the *peer-to-peer* layer. On top of it, an agent with
`role: coordinator` runs a `CoordinatorBehavior` that orchestrates a pool of
`role: worker` agents top-down. On a run it:

1. **Decomposes** the goal into subtasks. With a symbolic
   [HTN](https://en.wikipedia.org/wiki/Hierarchical_task_network) planner when
   the workflow sets an `htn_methods_file` (no LLM call for the resolved tasks),
   otherwise it decomposes the whole goal with the LLM.
2. **Spawns** worker agents at runtime, each with the full agent stack
   (memory, checkpointing, hooks, tools).
3. **Assigns** each subtask via a **capability + budget auction** — workers
   bid, the best fit wins.
4. Runs the workers in parallel, then stops and joins them at the end of the
   run.

Both the auction-based worker assignment and the coordinator role are
**shipped** on the live execute path. The symbolic HTN planner is **opt-in**:
it only runs when a workflow provides an `htn_methods_file`; with no methods
file the coordinator falls back to LLM decomposition.

## Memory tiers

Four memory tiers, plus checkpointing as a separate crash-recovery concern:

| Tier | What | Persistence |
|---|---|---|
| 1 — Session | live conversation transcript | in-memory |
| 2 — Daily log | append-only JSONL by date; agent-readable by date range via the `recall_timeframe` tool | disk (JSONL) |
| 3 — Core memory | named agent-editable blocks (`persona` / `human` / `project` by default), rendered into the prompt each turn; a `shared` block is visible across agents | disk (per-agent JSON) |
| 4 — Semantic | lossless vector recall — passive top-k injection + the agent-driven `recall_search` tool (Candle + all-MiniLM-L6-v2, 384-dim embeddings, hash fallback) | disk |

The agent curates Tier 3 itself via `core_memory_append` / `core_memory_replace`
/ `core_memory_set` tools. **Checkpointing** is separate from the tiers: it
snapshots the session to disk (bincode, `0600`, keep-last-3) so a restarted
agent restores its conversation transcript.

## Protocols

- **MCP** — the daemon connects to configured `mcp_servers` (stdio or
  streamable-http) at bootstrap, discovers their tools, and **executes them**:
  it keeps the client alive after discovery, and the shared tool executor
  routes an LLM's qualified `mcp__server__tool` call through to the live
  server. Agents can also be exposed *as* MCP tools via `axocoatl mcp serve`.
- **A2A** — **inbound** agent-to-agent interop: the daemon mounts
  `/.well-known/agent.json` and `/a2a/tasks`, dispatching tasks from remote
  agents to local ones. (There is no outbound A2A client yet — Axocoatl
  receives A2A tasks but does not delegate out.)

## Sandbox isolation

Directory sessions run inside a **hardened rootless podman container** — this
is the shipped isolation boundary. Session file/shell tools execute inside it;
hardening drops dangerous capabilities and sets `no-new-privileges`.

Other isolation tiers are not part of the default build. The Wasmtime/WASM tool
tier is an experimental opt-in (`--features wasmtime-sandbox`); the
OCI/Firecracker-class microVM tiers are feature-gated out entirely
(`firecracker-isolation` / `oci-isolation`) and remain stubs behind those
features.

## Crate map

`axocoatl-core` (types) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime, incl. the
coordinator role) · `axocoatl-memory` · `axocoatl-coordination` (the lattice +
the shipped HTN-planner and auction primitives the coordinator uses) ·
`axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` · `axocoatl-isolation`
(rootless podman sandbox; WASM tier is an experimental opt-in feature) ·
`axocoatl-daemon` · `axocoatl-server` · `axocoatl-cli`.

It all ships as a single ~26 MB release binary. (`axocoatl-graph` exists as a
standalone, experimental graph-validation crate, but it is not wired into the
runtime.)
