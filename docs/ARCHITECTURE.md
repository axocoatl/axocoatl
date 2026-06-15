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
2. **Compact context** automatically when the session approaches the model's
   window — old turns are summarized (raw archived to the Tier-2 daily log, so
   nothing is lost) instead of being dropped.
3. Build the request, injecting the agent's **core-memory blocks** (Tier 3) and
   the top-k **semantic recall** (Tier 4) for the turn.
4. **Token budget** pre-flight check (`abort` / `warn`) — the spend cap.
5. Call the agent's **provider** (Ollama, OpenAI, Anthropic, …).
6. Run any **tool calls** (built-in or MCP) with hooks, up to 10 iterations.
7. **Checkpoint** the session to disk (Tier 2) for crash recovery.

The agent curates its core-memory blocks (Tier 3) during the conversation; the
lossless raw is always preserved in Tiers 2 and 4.

## Token budgets

Per-agent `token_budget` with `per_call`, `per_execution`, and an
`overflow_policy`:

- `abort` — refuse the over-budget call and return a budget error (the default)
- `warn` — log and continue past the budget

Budgets are checked **before** the LLM call, so an over-budget request never
costs tokens. The `overflow_policy` is purely the **spend cap** — context
compaction toward the model window is automatic and independent of it.
(`summarize` is accepted as a deprecated alias for `warn`.)

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

## Coordinator role

Alongside emergent lattice coordination, an agent can take the **coordinator**
role (`role: coordinator`) for explicit hierarchical decomposition. Each
coordination pass (`CoordinatorBehavior`):

1. **Decompose** the goal into subtasks. With HTN methods configured, planning
   is symbolic — an `HtnPlanner` expands compound tasks via its methods and an
   `LlmFrontierResolver` resolves only the frontiers the methods don't cover.
   Without methods, the LLM decomposes the whole goal. Each subtask carries the
   tools it needs.
2. **Assign** each subtask to a worker by **auction** (`compute_bid` /
   `run_auction`) — best fit by tool-capability match and remaining token
   budget. If no pooled worker can cover a subtask's tools, an ad-hoc worker is
   spawned with exactly those tools, so a subtask is never forced onto an unfit
   worker.
3. **Delegate** the pending subtasks to workers **in parallel**. Each worker is
   a first-class agent — its own tools, checkpoints, core + semantic
   memory, and hooks — with a run-scoped actor name so repeated runs never
   collide.
4. **Synthesize** the workers' outputs back into one answer to the original
   goal, accounting for any subtasks that failed.

The pass is **resumable**: the plan and each completed subtask are checkpointed
(`OrchestrationState`), so a crash mid-run resumes where it left off instead of
re-doing finished work. Workers are always torn down after a pass — on success
and on every error path — so no actor or task leaks, and a fully failed worker
set surfaces an error rather than a hollow result. The underlying primitives
(`axocoatl-coordination`: lattice, HTN, auction) run in sub-microsecond time and
are independently tested.

## Memory tiers

| Tier | What | Persistence |
|---|---|---|
| 1 — Session | conversation transcript | in-memory |
| 2 — Checkpoint | agent state snapshots | disk (pruned to 3) |
| 3 — Core memory | agent-edited curated blocks | disk (JSON; per-agent + shared) |
| 4 — Semantic | neural vector recall | disk (embeddings) |

Tier 4 runs a pure-Rust neural embedding model (`all-MiniLM-L6-v2`, 384-dim) on
Candle — the ~90 MB model is downloaded once, with a feature-hash fallback when
it's unavailable. No external service, no network at inference time.

**Recall is hybrid.** Each turn the top-k Tier-4 hits are injected passively (the
baseline), and the agent can also *pull* on demand with two tools: `recall_search`
(semantic search over Tier 4) and `recall_timeframe` (read the Tier-2 daily log
for a date or range). A standing capability hint — plus a post-compaction note
pointing at the summary — tells the agent what's recallable, so the tools get
used instead of sitting idle. Passive injection, `top_k`, and the relevance
`min_score` are per-agent (`memory.recall` in config); passive can be turned off
to go fully agent-driven.

**Core memory is agent-managed.** Tier 3 is a small set of named, editable blocks
(`persona`, `human`, `project`, …) rendered into the system prompt every turn. The
agent curates them itself via `core_memory_append` / `core_memory_replace` /
`core_memory_set` as it learns durable facts (the MemGPT/Letta model — replacing
the old session-end fact extraction). Blocks are per-agent by default; a block
marked `shared` forms cross-agent team memory. This is the **curated top** of the
hierarchy — small and lossy by design, safe to rewrite because the lossless raw
stays in Tier 2 (daily log) and Tier 4 (semantic). Configure the block set per
agent under `memory.core`.

**Sleep-time consolidation.** A background loop (`consolidation.rs`, mirroring
supervision) periodically asks **idle** agents to consolidate. Each agent runs an
LLM "memory manager" pass — `on_consolidate`, triggered by an
`AgentMessage::Consolidate` and once more on graceful stop — that reviews recent
Tier-4 activity and **promotes durable facts into the right core block**, merging
duplicates and tightening wording within the char limits. It is **promotion-only**:
it reads Tier 4 and never evicts it. The agent itself decides whether it has been
idle long enough (the pass runs only past `idle_threshold_secs`), so a pass never
fires between a user's two messages. Tune under `consolidation` (`enabled`,
`idle_threshold_secs`, `interval_secs`).

## Protocols

- **MCP** — the daemon connects to configured `mcp_servers` (stdio or
  streamable-http) at bootstrap and exposes their tools to agents. Axocoatl is
  also an MCP **server**: `axocoatl mcp serve` runs over stdio and exposes each
  agent as an `agent_<id>` tool.
- **A2A** — agent-to-agent interop for cross-framework workflows, reachable over
  `GET /.well-known/agent.json` and `POST /a2a/tasks`.

Runnable examples: [`mcp-bridge`](../examples/mcp-bridge) (consume an MCP tool
over stdio, expose agents as an MCP server) and [`a2a-server`](../examples/a2a-server)
(publish an agent card and call it from a client, in-process).

## Security model

A session runs the agent's tools inside a **rootless, daemonless Podman
container**, not on the host. The threat model is deliberately narrow, and
stated plainly so you know what it does and doesn't cover.

**What the sandbox contains — the blast radius of a mistaken or misbehaving
agent:**

- **Filesystem.** Only the session's working directory is bind-mounted into the
  container (`{dir}:{dir}:rw`). Nothing else of the host is visible — not your
  home directory, SSH keys, or sibling projects. A destructive command
  (`rm -rf`, a bad `git reset`) can only reach that one directory.
- **Privileges.** The container runs with `--security-opt=no-new-privileges` and
  drops the escape/recon capabilities (`SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN`,
  `NET_RAW`, `DAC_READ_SEARCH`, …), so a setuid binary can't escalate and the
  classic namespace/mount escape levers are gone.
- **Network.** Untrusted runs start with `--network none` — no outbound
  connections at all. Bridged networking is opt-in, per policy.
- **Resources.** Memory, CPU, and PID caps (2 GB / 2 CPUs / 512 pids) bound a
  runaway loop or fork bomb, where the host's cgroup delegation allows it.

**What it does NOT solve — and we won't pretend otherwise:**

- **Prompt injection.** If the agent reads malicious instructions from a file, a
  web page, or tool output, the sandbox does not stop it from *acting* on them
  inside its workspace and its allowed network. Isolation bounds the blast
  radius; it is not a defense against an agent being talked into the wrong
  thing. Keep secrets out of the workspace and prefer `--network none` for
  untrusted inputs.
- **Host kernel / Podman bugs.** Container isolation is only as strong as the
  host kernel and Podman underneath it. A kernel-level container-escape CVE is
  outside our control.
- **What you explicitly grant.** Bridged networking, mounted credentials, or a
  permissive tool policy widen the surface — by your choice.

Report security issues per [SECURITY.md](../SECURITY.md).

## Crate map

`axocoatl-core` (types) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime) ·
`axocoatl-memory` · `axocoatl-coordination` (lattice/HTN/auction) ·
`axocoatl-graph` · `axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` ·
`axocoatl-isolation` (WASM) · `axocoatl-daemon` · `axocoatl-server` ·
`axocoatl-cli`.
