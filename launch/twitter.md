# Twitter / X

Thread of 9. Each tweet ≤ 270 characters so they render cleanly with
breathing room. No emoji. No exclamation marks (per BRAND.md).

---

## Tweet 1

We just shipped Axocoatl v0.1.0 — an open-source agentic runtime in
Rust.

Real workflows. No theater.

One 13 MB binary. Your hardware, your LLM, your data.

https://axocoatl.ai

(thread)

---

## Tweet 2

What it's actually for:

Most agent tooling optimizes for the aesthetic of AI work — chat
interfaces, demo videos, glowing terminals.

Axocoatl optimizes for the unglamorous reality. Agents that run for
months, persist state, follow real workflows, and finish the work.

---

## Tweet 3

What that looks like in practice:

– A runtime, not a framework
– Actor-supervised, checkpointed agents
– Stigmergic event lattice for coordination (no central scheduler)
– Survives restarts via hot checkpoints
– Runs as a system service

---

## Tweet 4

The lattice is the part I'm most proud of.

Agents declare `depends_on` and publish events when they finish work.
Downstream agents activate when their thresholds are crossed.

No orchestrator. The DAG is whatever the dependency graph implies.

→ https://axocoatl.ai/concepts

---

## Tweet 5

Provider matrix is wide:

– Ollama (default, local)
– OpenAI, Anthropic, Mistral, Gemini
– OpenRouter (every model behind one key)

Each agent picks its own. Local model for cheap classification,
frontier model for the one agent that needs it.

---

## Tweet 6

Memory is four-tier, hot to cold:

1. Session (in-process)
2. Checkpoint (durable snapshots)
3. Long-term (key/value)
4. Semantic (neural — embedded BERT via Candle, pure Rust)

All local. No data leaves your box unless you wire it to.

---

## Tweet 7

Dashboard is vanilla HTML + Web Components, no React.

Sessions cockpit, Studio lattice canvas, Automations editor, Chat,
Files, Skills, MCP. macOS Finder–shaped because that's what a real
control panel looks like.

---

## Tweet 8

Concrete workflows we're running:

– Release notes drafted every Monday at 9am
– Support triage on incoming email
– Daily briefings landing in inbox at 8am
– Bug bisection in directory sessions
– Contract review on uploads

Real workflows. Not demos.

---

## Tweet 9

The runtime is Apache-2.0. Zero telemetry. Air-gappable.

If you want to try it:

  curl -fsSL https://axocoatl.ai/install.sh | sh

Repo: https://github.com/axocoatl/axocoatl
Docs: https://docs.axocoatl.ai

Thanks for reading.
