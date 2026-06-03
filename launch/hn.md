# Show HN

## Title

Show HN: Axocoatl – an agentic runtime that runs as a system service

## URL

https://axocoatl.ai

## Text

Hi HN. I'm the author of Axocoatl, an open-source Rust runtime for
self-coordinating multi-agent systems.

The short version: most agent tooling today optimizes for the
*aesthetic* of AI work — chat interfaces, demo videos, "I'm building
ten agents to write my blog posts." It looks productive. In practice
the agent has no memory, no supervision, and disappears the first time
the process dies. Axocoatl is the opposite stance — a runtime, not a
framework, that runs on your hardware, supervises a constellation of
agents, persists their state through restarts, and routes work between
them through a coordination layer called the event lattice.

Concretely:

- One 25 MB Rust binary. No Python venv, no Docker compose, no cloud
  account. `curl … | sh`, `axocoatl doctor`, `axocoatl dev`, open the
  dashboard.
- Agents are `ractor` actors. Each has its own mailbox, token budget,
  and four-tier memory (in-process / checkpoint / long-term / neural).
  Crashes get supervised restarts. State survives.
- Coordination is stigmergic: agents declare `depends_on` chains and
  publish `TaskCompleted` events. The lattice wakes downstream agents
  when their threshold is crossed. No central scheduler.
- Skills declare what events they emit and react to; the lattice
  routes them automatically.
- Directory sessions: a sandboxed work surface, locally, inside a
  podman container. Close the laptop, open it tomorrow, the session
  is still there.
- Providers: Ollama (local), OpenAI, Anthropic, Mistral, Gemini, and
  OpenRouter. Per-agent selection. Local model for one agent, frontier
  model for another, same workflow.
- Apache-2.0. Zero telemetry. Air-gappable.

Concept page (with a scripted lattice you can watch pulse):
https://axocoatl.ai/concepts

Why I built this: I run a small team that ships software, and every
existing agent framework I tried either (a) couldn't survive a laptop
sleep cycle or (b) required spinning up a cloud account I didn't want.
The work I actually want agents to do is the recurring background
stuff — release notes on Monday, support triage on incoming email,
daily briefings — and that work needs persistence, supervision, and
scheduling out of the box.

Stack:
- Rust workspace: `ractor` for actors, `axum` for the server, `candle`
  for neural Tier-4 memory (pure-Rust BERT embeddings, no ONNX), the
  dashboard is vanilla HTML + Web Components + Monaco.
- Test coverage: 340+ tests across the workspace, all green.

Tried to ship something that looks like a tool, not a demo. Comments
and code review very welcome.
