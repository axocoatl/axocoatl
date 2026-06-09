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

**On the roadmap** — additional coordination primitives are built and tested in
`axocoatl-coordination` (sub-microsecond) but not yet wired into the runtime:

- **HTN planner** — symbolic task decomposition without LLM calls.
- **Auction** — deterministic agent selection by tool capability, load, and
  remaining token budget.

## Memory tiers

| Tier | What | Persistence |
|---|---|---|
| 1 — Session | conversation transcript | in-memory |
| 2 — Checkpoint | agent state snapshots | disk (pruned to 3) |
| 3 — Long-term | distilled facts | disk (bincode) |
| 4 — Semantic | neural vector recall | disk (embeddings) |

Tier 4 runs a pure-Rust neural embedding model (`all-MiniLM-L6-v2`, 384-dim) on
Candle — the ~90 MB model is downloaded once, with a feature-hash fallback when
it's unavailable. No external service, no network at inference time.

## Protocols

- **MCP** — the daemon connects to configured `mcp_servers` (stdio or
  streamable-http) at bootstrap and exposes their tools to agents. Axocoatl is
  also an MCP **server**: `axocoatl mcp serve` runs over stdio and exposes each
  agent as an `agent_<id>` tool.
- **A2A** — agent-to-agent interop for cross-framework workflows, reachable over
  `GET /.well-known/agent.json` and `POST /a2a/tasks`.

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
