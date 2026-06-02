# Axocoatl

**The Rust runtime for self-coordinating multi-agent systems.**

[![CI](https://github.com/axocoatl/axocoatl/actions/workflows/ci.yml/badge.svg)](https://github.com/axocoatl/axocoatl/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/axocoatl-cli.svg)](https://crates.io/crates/axocoatl-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

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

## Core concepts

- **Agents** — persistent `ractor` actors with a provider, tools, 4-tier
  memory, and a token budget. Survive restarts via checkpointing.
- **Stigmergic coordination** — agents publish `TaskCompleted` events; an
  `EventLattice` accumulates pheromone signals and activates downstream agents
  when thresholds are crossed. No scheduler, no glue code.
- **Workflows** — declarative multi-agent DAGs via `depends_on` / `entry_point`.
- **Providers** — Ollama, OpenAI, Anthropic, Mistral, Gemini. No lock-in.
- **Protocols** — MCP (consume & expose tools) and A2A (agent interop).

See the [docs site](https://docs.axocoatl.ai) for the full picture, the
[marketing site](https://axocoatl.ai) for the positioning, or
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md) for the in-repo
quick reference.

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

Runnable, mock-LLM (no keys needed) — see [`examples/`](examples/):
`research-assistant`, `code-reviewer`, `customer-support`.

## Build from source

```bash
git clone https://github.com/axocoatl/axocoatl
cd axocoatl
cargo build --release          # binary: target/release/axocoatl
cargo test --workspace         # 318+ tests
```

## License

Apache-2.0 — see [LICENSE](LICENSE). Changes: [CHANGELOG.md](CHANGELOG.md).
