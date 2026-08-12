---
title: Architecture
description: "The mental model behind the one workbench: sessions, Automations, events, actors, memory, and isolation."
---

# Axocoatl Architecture

A practical overview of how Axocoatl's one workbench runs and coordinates agents.

## The big picture

```
            ┌─────────────────────────── axocoatl daemon ───────────────────────────┐
 App / CLI  │  ProviderRegistry   AgentRegistry   EventLattice   McpToolRegistry     │
 HTTP / WS ─┼─▶ (per-agent LLMs)  (ractor actors) (skills/events)  (MCP tools)         │
    / IPC   │        │                 │                │                            │
            │        └──────── DefaultAgentBehavior ─────┘                            │
            │       session mem → memory → budget → LLM → tools → checkpoint          │
            └────────────────────────────────────────────────────────────────────────┘
```

The **daemon** (`axocoatl-daemon`) bootstraps everything: providers, agents
(spawned as `ractor` actors), the event lattice, MCP connections, and the
canonical Automation trigger runtime. Both `axocoatl dev` and `axocoatl serve`
expose the Unix-socket IPC server and HTTP/browser app from the same daemon
state; `serve` is also what the installed background service runs.

## Product surface

The browser app at `/` is the only supported interactive browser route. A
workspace authorizes a project directory, and a session is persistent work and
chat anchored to that directory. Files, editor, terminal, browser, activity,
attempt comparison, git, and agent graph open around that chat. Agents, Skills,
MCP servers, and Automations live in Settings.

The visual shell does not erase the runtime's distinct state owners. Directory
sessions, lightweight chats, Automation runs, and attempt sets have different
transcript and lifecycle boundaries; changes at those seams must verify
persistence, reconnect, cancellation, and cleanup end to end.

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

- `abort` — refuse the over-budget call
- `warn` — log and continue

Budgets are checked **before** the LLM call, so an over-budget request never
costs tokens. Both the `per_call` (single-call) and `per_execution`
(cumulative) limits are enforced pre-flight. (`summarize` is a deprecated YAML
alias that now maps to `warn` — context compaction is automatic and
independent of the spend budget; see [Memory tiers](#memory-tiers).)

## Coordination paths

Axocoatl has three related paths; they should not be collapsed into one claim:

- **Automations.** `AutomationStore` is the canonical persisted source for
  manual, fixed-interval, event-triggered, and Skill-triggered graphs. The
  explicit DAG executor schedules ready nodes and records node/run outcomes.
  Legacy `workflows:`, `schedules:`, and `proactive:` YAML seed this store only
  when its canonical file does not exist. The workflow CLI and HTTP routes are
  compatibility views of manual Automation records.
- **Multi-agent directory sessions.** A `Lattice` session uses a selected legacy
  workflow as its agent-membership definition. Session-scoped actors run in
  dependency order inside one sandbox and stream lifecycle frames under the
  session id. This is a bounded foreground path, not a background YAML-owned
  workflow runner.
- **Event lattice.** Skills and runtime components publish typed events. The
  Automation trigger runtime, activity timeline, and configured webhooks
  subscribe to that feed. The coordination crate still exposes reusable
  pheromone/signal primitives, but the product daemon does not consume their
  returned activation ids through a second execution loop.

### Hierarchical coordinator (role-based orchestration)

Separately, an agent with `role: coordinator` runs a `CoordinatorBehavior` that
orchestrates a pool of `role: worker` agents top-down. On a run it:

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

Both the auction-based worker assignment and the coordinator role are on the
live execute path. The symbolic HTN planner is **opt-in**:
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

Local directory sessions use a **hardened rootless Podman container** by
default. Session file/shell tools execute inside it; hardening drops dangerous
capabilities and sets `no-new-privileges`. Networking is bridged by default so
package installation and development servers work; set `sandbox.network: none`
when session tools must have no outbound route. An E2B-compatible remote
backend is an explicit per-session choice.

The isolation crate includes a Wasmtime implementation in its default feature
set, but the daemon's agent tool executor does not expose that backend. Do not
present WASM, OCI, or Firecracker as selectable product isolation tiers.

## Crate map

`axocoatl-core` (types) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime, incl. the
coordinator role) · `axocoatl-memory` · `axocoatl-coordination` (the lattice +
the shipped HTN-planner and auction primitives the coordinator uses) ·
`axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` · `axocoatl-isolation`
(local Podman and optional E2B session backends; an unwired Wasmtime
implementation also lives in the crate) ·
`axocoatl-daemon` · `axocoatl-server` · `axocoatl-cli`.

It all ships as a single release binary. (`axocoatl-graph` exists as a
standalone, experimental graph-validation crate, but it is not wired into the
runtime.)
