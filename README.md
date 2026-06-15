# Axocoatl

**The Rust runtime for self-coordinating multi-agent systems.**

[![CI](https://github.com/axocoatl/axocoatl/actions/workflows/ci.yml/badge.svg)](https://github.com/axocoatl/axocoatl/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/axocoatl-cli.svg)](https://crates.io/crates/axocoatl-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

<p align="center">
  <img src="docs/img/demo.gif" alt="Axocoatl crash-restart — kill the server mid-run and the agent resumes from its last checkpoint, fully local" width="760">
</p>

<p align="center"><em>Kill the server mid-task — the agent restarts from its last checkpoint, not from zero. 100% local.</em></p>

Axocoatl runs persistent AI agents that coordinate through a **stigmergic event
lattice** — agents activate when their dependencies complete, driven by
pheromone-style signals with no central orchestrator. Built in Rust on the
`ractor` actor model: low memory, fast cold start, provider-agnostic.

---

## 60-second quickstart

```bash
# 1. Install (no Rust toolchain required)
curl -fsSL https://raw.githubusercontent.com/axocoatl/axocoatl/main/scripts/install.sh | sh

# 2. Interactive setup wizard — picks a provider, scaffolds a project
axocoatl onboard

# 3. Check your environment
axocoatl doctor

# 4. Start the daemon + API, then chat
axocoatl dev
axocoatl chat -a assistant
```

Prefer Cargo? `cargo install axocoatl-cli` (requires Rust 1.82+).

> **Skipping `onboard`?** Copy [`axocoatl.example.yaml`](axocoatl.example.yaml)
> to `axocoatl.yaml` — two agents and one workflow, fits on one screen.
> The full `axocoatl.yaml` shipped in the repo is the larger demo (12 agents,
> scheduled runs, MCP servers).

---

## Why Axocoatl

| Capability | Axocoatl | AutoAgents | CrewAI |
|---|:--:|:--:|:--:|
| Language / runtime | Rust / actors | Rust / actors | Python |
| **Stigmergic coordination** (no orchestrator) | ✅ | ❌ | ❌ |
| HTN symbolic planning | ✅ | ❌ | ❌ |
| Auction-based agent selection | ✅ | ❌ | ❌ |
| Per-agent token budgets | ✅ | ❌ | partial |
| 4-tier persistent memory + checkpointing | ✅ | partial | partial |
| MCP client + server | ✅ | partial | ✅ |
| A2A protocol | ✅ | ❌ | ❌ |
| Provider-agnostic (Ollama/OpenAI/Anthropic/…) | ✅ | ✅ | ✅ |
| Interactive onboarding + `doctor` | ✅ | ❌ | ❌ |

The differentiator is the **coordination layer**: define agents with
`depends_on`, and the event lattice cascades work through them automatically.

```yaml
agents:
  - id: researcher
    provider: ollama
    model: llama3.2
    depends_on: []
  - id: summarizer
    provider: ollama
    model: llama3.2
    depends_on: [researcher]   # activates when researcher completes

workflows:
  - id: research-and-summarize
    agents: [researcher, summarizer]
    entry_point: researcher
```

```bash
axocoatl workflow run research-and-summarize -i "What is photosynthesis?"
```

---

## See it work

**Give it a goal — it builds the team.** A coordinator agent decomposes the goal
into subtasks, spawns a worker to fit each one, and runs them in parallel. No
orchestration code, no glue.

<p align="center"><img src="docs/img/coordinator.gif" alt="A coordinator agent decomposes a goal into five subtasks and spawns a worker for each, running in parallel" width="760"></p>

**Tell it once — it remembers.** Store a preference, open a brand-new
conversation, and it still knows. Agent-editable core memory that persists
across runs.

<p align="center"><img src="docs/img/memory.gif" alt="An agent stores a preference to core memory, then recalls it in a separate conversation" width="760"></p>

**It never phones home.** Every socket the daemon opens is `127.0.0.1`; the only
outbound call is your local model. Zero telemetry, zero external connections.

<p align="center"><img src="docs/img/no-phone-home.gif" alt="A live lsof of the running daemon shows every socket is 127.0.0.1, with zero external connections" width="760"></p>

---

## Core concepts

- **Agents** — persistent `ractor` actors with a provider, tools, 4-tier
  memory, and a token budget. Survive restarts via checkpointing.
- **Hybrid memory recall** — relevant past exchanges are injected each turn, and
  the agent can also pull on demand: `recall_search` (semantic search over past
  sessions) and `recall_timeframe` (read a day's activity log). Tunable per agent.
- **Agent-managed core memory** — editable blocks (`persona`, `human`, `project`,
  …) the agent curates via tools and that render into its prompt each turn (the
  MemGPT/Letta model). Per-agent by default, shareable across agents. A
  background "sleep-time" pass consolidates idle agents' memory automatically.
- **Stigmergic coordination** — agents publish `TaskCompleted` events; an
  `EventLattice` accumulates pheromone signals and activates downstream agents
  when thresholds are crossed. No scheduler, no glue code.
- **Coordinator role** — for explicit hierarchical work, an agent with
  `role: coordinator` decomposes a goal into subtasks (HTN or LLM), auctions them
  to worker agents, runs them in parallel, and synthesizes the results. The pass
  is resumable via checkpointing.
- **Workflows** — declarative multi-agent DAGs via `depends_on` / `entry_point`.
- **Providers** — Ollama, OpenAI, Anthropic, Mistral, Gemini, OpenRouter. No lock-in.
- **Protocols** — MCP (consume & expose tools) and A2A (agent interop).

See the [docs site](https://docs.axocoatl.ai) for the full picture, the
[marketing site](https://axocoatl.ai) for the positioning, or
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md) for the in-repo
quick reference.

---

## Roadmap

- **Stronger sandbox isolation tiers** — the shipped sandbox is a hardened
  rootless Podman container (capabilities dropped, no-new-privileges,
  network-isolatable); microVM-class isolation (Firecracker) is planned.

---

## CLI

```
axocoatl onboard                 Interactive setup wizard
axocoatl doctor                  Environment / dependency health check
axocoatl init <name>             Scaffold a project non-interactively
axocoatl validate <config>       Validate a config file
axocoatl dev | serve             Run daemon (+ IPC) / production server
axocoatl chat -a <agent>         Interactive chat
axocoatl workflow list | run     Inspect / execute multi-agent workflows
axocoatl agents list|status|restart
axocoatl tokens report           Per-agent token usage
axocoatl mcp servers|tools       Inspect connected MCP servers/tools
```

## HTTP API

```
GET  /health                          POST /api/agents/{id}/execute
GET  /api/agents                       GET  /api/agents/{id}/status
POST /api/agents/{id}/restart          GET  /api/tokens/report
GET  /api/workflows                    POST /api/workflows/{id}/execute
GET  /api/mcp/servers                  GET  /api/mcp/tools
GET  /ws   (WebSocket streaming)
```

## Examples

Every example is runnable with a mock LLM — **no API keys needed** — unless
noted. See [`examples/`](examples/).

**Coordination & planning**
- [`stigmergic-workflow`](examples/stigmergic-workflow) — the `EventLattice` + `depends_on` DAG. The running order *emerges* from pheromone signals crossing thresholds; no orchestrator decides it.
- [`skills-lattice`](examples/skills-lattice) — event-driven Skills: one event fans out to every agent that `reacts_to` it (`emits`/`reacts_to`), distinct from a fixed DAG.
- [`htn-planner`](examples/htn-planner) — symbolic HTN decomposition; compound tasks expand via methods and only unresolved frontiers reach the LLM.
- [`crash-recovery`](examples/crash-recovery) — kill a multi-step workflow mid-run and resume from the checkpoint; completed steps are not re-run.

**Memory & providers**
- [`memory-recall`](examples/memory-recall) — agent-managed core memory, semantic recall, and sleep-time consolidation (Tiers 3–4); runs offline.
- [`multi-provider`](examples/multi-provider) — per-agent provider selection: a cheap local model for simple steps, a frontier model for the hard one, with a per-tier cost breakdown.

**Tools, protocols & integration**
- [`tool-hooks`](examples/tool-hooks) — pre/post tool hooks that deny a path-traversal write, audit every call as JSON, and let the agent recover.
- [`mcp-bridge`](examples/mcp-bridge) — call an external MCP tool over stdio through the real `McpToolRegistry`; plus how to expose agents as an MCP server.
- [`a2a-server`](examples/a2a-server) — expose an agent over the A2A protocol (agent card + task endpoint) and call it from a client, in-process.
- [`sandbox-session`](examples/sandbox-session) — the rootless Podman sandbox for agent tool execution: threat model, config knobs, and a live integration test (needs Podman).

**Autonomy & config**
- [`proactive-agents`](examples/proactive-agents) — agents that fire on a schedule or on an event (here, reacting to `AgentFailed`), not on a user prompt.
- [`configs/`](examples/configs) — a gallery of minimal YAML configs for common recipes (research pipeline, feature dev, incident response, local-only, MCP). No Rust.

**Foundations**
- [`research-assistant`](examples/research-assistant), [`code-reviewer`](examples/code-reviewer), [`customer-support`](examples/customer-support) — agent coordination, token budgets, and session/checkpoint memory.

## Build from source

```bash
git clone https://github.com/axocoatl/axocoatl
cd axocoatl
cargo build --release          # binary: target/release/axocoatl
cargo test --workspace         # 430+ tests
```

## License

Apache-2.0 — see [LICENSE](LICENSE). Changes: [CHANGELOG.md](CHANGELOG.md).
